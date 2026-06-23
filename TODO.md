# Roadmap / TODO

> Working backlog for the engine. Ordered **roughly** by dependency → importance →
> difficulty, grouped into phases. This is a living file — re-rank freely. The phases
> are a suggested critical path, **not** rigid gates; items marked _(parallel)_ can
> proceed independently.
>
> **Completed items are collapsed to a one-line ✅ entry + a pointer to the spec doc that
> records the detail.** The full design, the *why*, the error codes, the golden-fixture
> names, and the divergence ledgers live in `spec/design/*` and git history — not here.
> Open `[ ]` items (including follow-ups hoisted out of done items, marked _follow-on:_)
> are the live backlog; size tags `_(size: …)_` are kept on open items only.
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

## Phase 0 — Meta / housekeeping

- [x] **Name the project** — settled on **`jed`** (was the placeholder `abide`); swept code,
      docs, on-disk magic (`ABDB`→`JEDB`), file extension (`.adb`→`.jed`), devcontainer ids.

---

## Phase 1 — Foundations: spec backfill + the expression substrate

> Highest leverage, mostly low difficulty. These unblock nearly every later feature and
> closed gaps in the canonical artifact itself.

- [x] **Backfill the EBNF grammar** — the shared contract the hand-written parsers conform to.
      → [grammar.ebnf](spec/grammar/grammar.ebnf), [grammar.md](spec/design/grammar.md)
- [x] **Author the function / operator catalog** — operator result types + NULL behavior as
      data, family-based schema + coherence checker. → [catalog.toml](spec/functions/catalog.toml),
      [functions.md](spec/design/functions.md)
- [x] **Codegen "middle path"** — catalog → per-language operator descriptor tables (data only;
      parser/executor/evaluator stay hand-written), drift-gated by `rake verify`.
      → [gen_catalog.rb](scripts/gen_catalog.rb), [codegen.md](spec/design/codegen.md)
  - [ ] _follow-on:_ extend the generator to types/errors.
    - [x] **errors** — `SqlState` enum + code mapping + `ERRORS` table generated per core from
          [registry.toml](spec/errors/registry.toml); hand-written `EngineError` scaffolding
          consumes it. Drift-gated by `rake verify`. → [gen_errors.rb](scripts/gen_errors.rb)
    - [ ] **types** — scalars (ragged fields + enum identity threaded through the codec) remain.
- [x] **Resolve integer-literal typing** — context-adaptive untyped constants (adapt to the
      column/CAST target, trap `22003` out of range, default i64). → [types.md §6](spec/design/types.md)
- [x] **General expression evaluator** — unified recursive `Expr` (Column/Literal/Cast/Unary/
      Binary/IsNull), one-function-per-precedence-level parser, shared by WHERE + the SELECT list.
- [x] **Integer arithmetic `+ - * / %` + unary `-`** — trap-on-overflow `22003` at the result
      type's boundary, `/`/`%`-by-zero `22012`, result types from the promotion tower.
- [x] **`boolean` scalar (expression-only)** — `TRUE`/`FALSE` literals, comparison/logical
      results, render tag `B`. (Storable boolean is Phase 3.)
- [x] **Logical connectives `AND`/`OR`/`NOT`** — three-valued (Kleene) truth tables.
- [x] **`IS [NOT] DISTINCT FROM`** — NULL-safe equality (`null = "null_safe"`).
      → [functions.md §3](spec/design/functions.md)
- [x] **Cost-accounting seam** — a deterministic `Meter` threading the executor / evaluator /
      storage reads, a data-defined unit schedule, the `# cost:` corpus directive asserting
      byte-identical accrued cost cross-core. Ceiling+abort (`54P01`) and a real `page_read` unit
      have since landed. → [cost.md](spec/design/cost.md), [schedule.toml](spec/cost/schedule.toml) _(§13)_
  - [ ] _follow-on:_ per-operator `cost` weights.

---

## Phase 2 — Make it feel like SQL (core query/DML completeness)

> Builds directly on the Phase 1 expression substrate.

- [x] **Select-list expressions + `*` + column aliases (`AS`)** — output column naming as a
      cross-core contract (the `# names:` directive). → [grammar.md §8](spec/design/grammar.md)
- [x] **`LIMIT` / `OFFSET`** — either order, non-negative integer literal (negative →
      `2201W`/`2201X`), applied after ORDER BY before projection. → [grammar.md §9](spec/design/grammar.md)
- [x] **Richer `ORDER BY`** — multiple keys, per-key `ASC`/`DESC`, `NULLS FIRST|LAST` (PG
      NULL-largest default). → [grammar.md §10](spec/design/grammar.md)
  - [ ] _follow-on:_ ordinal / expression / alias sort keys.
- [x] **`ORDER BY` satisfied by primary-key scan order** — a single-table, non-aggregate,
      non-`DISTINCT` `SELECT` whose `ORDER BY` is an `ASC` prefix of the PK columns (sorting by each
      column's stored key order) elides the sort and streams the scan; with a `LIMIT` it
      short-circuits a top-N (`storage_row_read` drops to the rows read). Composes with a PK range
      bound; a collated PK is stored in collation order, so a collated `ORDER BY` is satisfied with
      no in-memory re-sort and no `collate` units. Capability `query.order_by_pk_scan`.
      → [cost.md §3](spec/design/cost.md), [grammar.md §10](spec/design/grammar.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ `DESC` (reverse scan); **secondary-index
        order** (walk the index tree + point-lookup — the general non-PK collated-`ORDER BY` payoff,
        needs a variable-width key-suffix skip); `DISTINCT`; multi-table joins.
- [x] **`DISTINCT`** — NULL-safe dedup of projected rows, after ORDER BY before LIMIT; PG
      restriction on ORDER BY keys (`42P10`). → [grammar.md §11](spec/design/grammar.md)
- [x] **FROM-less `SELECT`** — `SELECT 1` over one virtual zero-column row.
      → [grammar.md §34](spec/design/grammar.md)
- [x] **Predicate forms — `IN (list)`, `BETWEEN`, `LIKE`, `CASE`** — IN/BETWEEN desugar to
      `=`/`OR`/`AND`/`NOT`; LIKE is a code-point matcher (`%`/`_`, `\` escape, `22025`); CASE is
      the engine's first lazy expression. → grammar.md §20–§23
  - [x] **`ILIKE`** — case-insensitive `LIKE` (landed with collation Slice 3e).
  - [x] **Regular expressions** — `~` `~*` `!~` `!~*` operators + `regexp_replace` / `regexp_match`
        functions, jed's own RE2-able flavor (a hand-written linear-time **Pike VM**, ReDoS-immune,
        NOT PostgreSQL-flavor-compatible). Code-point matching; `2201B` malformed / `54001`
        over-large program (`MAX_REGEX_PROGRAM`); `regex_compile`/`regex_step` cost units; cross-core
        program/match-vector fixtures. All three cores byte-identical, oracle-clean on the PG-agreeing
        subset. → [regex.md](spec/design/regex.md)
  - [ ] _follow-on:_ LIKE `ESCAPE 'c'`; `SIMILAR TO` (deliberately excluded — the SQL-standard
        surface); set-returning `regexp_matches` / `regexp_split_to_table`; the Oracle-compat
        `regexp_count`/`instr`/`substr`/`like`; Unicode-property char classes (`\p{…}`),
        backreferences + lookaround (permanently out — they would break the linear-time guarantee).
- [x] **Aggregates `COUNT`/`SUM`/`MIN`/`MAX`/`AVG` + `GROUP BY` + `HAVING`** — first
      function-call syntax, whole-table + grouped aggregation, PG widening (SUM int→i64/decimal,
      AVG→decimal), grouping-error `42803`. → [aggregates.md](spec/design/aggregates.md)
  - [ ] _follow-on:_ `COUNT(DISTINCT x)`, `SELECT DISTINCT` in an aggregate query, GROUP BY by
        expression/ordinal/alias, functional-dependency grouping, `GROUPING SETS`/`FILTER`/ordered-set.
- [x] **Window functions (`OVER`)** — ✅ **COMPLETE (S0–S10, all three cores) + the sliding/sharing
      optimization.** Per-row values folded over a related row set in a dedicated **window stage**
      (after `GROUP BY`/`HAVING`, before `ORDER BY`/`LIMIT`). row_number/rank/dense_rank/percent_rank/
      cume_dist/ntile, lag/lead, the aggregates as window functions (running + explicit
      ROWS/RANGE/GROUPS frames + value offsets + EXCLUDE), first_value/last_value/nth_value, the
      `WINDOW` named-window clause + `OVER name` + base-window extension, combination with GROUP
      BY/aggregates, and a collation-honoring `ORDER BY`. The window stage **shares one partition/sort
      pass** across specs with an identical definition and **slides** a no-EXCLUDE aggregate's frame
      accumulator (expanding = fold-once for every aggregate; moving `count` = un-fold the left edge) —
      cost-lowering only, lowering `window_frame_step`/`operator_eval` cross-core-identically (a NoREC
      `window` relation + the `window_running_sum`/`window_moving_count` benchmarks guard it). New codes
      42P20/22013/22014/22016; cost units `window_result`/`window_frame_step`; the `[[window]]` catalog
      array. Divergences: within-partition order fully resolved (D1), percent_rank/cume_dist → decimal
      not float8 (D2), float-keyed RANGE frames 0A000 (D3). Deferred follow-ons: prefix-compatible
      (not just identical) sort sharing, a safely-invertible moving `sum`/`avg`/`min`/`max`/float
      slide, RANGE offsets over a float (D3) / timestamp / date key, general-expression window keys,
      `FILTER`/`WITHIN GROUP`, `IGNORE NULLS`. → [window.md](spec/design/window.md)
- [x] **Scalar functions `abs` / `round`** — first named per-row functions (`kind = "function"`).
      → [functions.md §9](spec/design/functions.md)
  - [ ] _follow-on:_ `ceil`/`floor`/`mod`/`sign`, text `length`/`lower`/`upper`, a general implicit
        argument-coercion pass.
- [x] **Named + optional (DEFAULT) function arguments** — PG named notation (`f(name => value)`) +
      DEFAULT params, driven by `make_interval`. → [functions.md §11](spec/design/functions.md)
  - [ ] _follow-on:_ `make_timestamp`/`make_timestamptz`; general non-integer/UDF defaults;
        `VARIADIC` (blocked on the array type).
- [x] **Multi-row `INSERT`** (`VALUES (..),(..)`) — two-phase / all-or-nothing.
      → [grammar.md §12](spec/design/grammar.md)
- [x] **`INSERT ... SELECT`** — query rows through the same two-phase validation; arity `42601` /
      type `42804` checked up front (even over an empty source). → [grammar.md §24](spec/design/grammar.md)
- [x] **`DROP TABLE`** — removes definition + rows; missing → `42P01`; zero cost.
      → [grammar.md §13](spec/design/grammar.md)
  - [ ] _follow-on:_ `IF EXISTS`, multi-table `DROP TABLE a, b`, `CASCADE`/`RESTRICT`.

---

## Phase 3 — The type system as the product (the differentiator, §4)

> The **real type system** is the product (§4) — PostgreSQL's behavior, stricter than its
> typing, nothing like SQLite's runtime affinity. `boolean`, `text` (collation `C`), `decimal`,
> `timestamp`/`timestamptz`, `interval`, `bytea`, `uuid`, and `f32`/`f64` are done;
> `json` and `array` are the remaining headline items.

- [x] **Storable `boolean` column type** — on-disk type code 5, `bool-byte` codec, comparison +
      ORDER BY (false < true, NULLs last); boolean in a `PRIMARY KEY`/index (`bool-byte` key
      encoding) and explicit `boolean⇄i32` casts (i32 only; `bool⇄i16`/`i64` a forbidden `42804`)
      both landed. → [types.md §9](spec/design/types.md), [casts.toml](spec/types/casts.toml)
- [x] **`text` + ONE collation (`C`)** — UTF-8 byte/code-point order, on-disk type code 4, first
      operator overload (the UTF-8-vs-UTF-16 ordering trap handled in TS); text in a `PRIMARY KEY`/
      index/UNIQUE via the `text-terminated-escape` key encoding (oversized text key `0A000`).
      → [types.md §11](spec/design/types.md), [encoding.md §2.4](spec/design/encoding.md)
  - [ ] _follow-on:_ `varchar(n)` length limits (`22001`); runtime non-literal text→T casts;
        string functions (`||`, `length`, `lower`/`upper`, `substring`).
  - [x] _follow-on:_ **linguistic collation (`COLLATE` / per-column / per-db default / UCA)** — ✅
        slice 1 (a–e) landed: jed-owned UCA executor + compiler, `COLLATE` expr/`ORDER BY`,
        per-column + per-db default, collated keys; deterministic collations only.
        → [collation.md](spec/design/collation.md)
        - [ ] **slice 2 — reference-only / vendored-tier pivot** (design revised, **not yet built**):
              flip from "vendor nothing, bake into the file by default" to **vendor the compiled
              tables into each core** at an embedder-chosen footprint tier (`C`-only / non-CJK /
              everything) and have the **file reference them by name + `(unicode, cldr)` version,
              never baking a table**. `ExtractHostCollation`/`CompileCollation` become **build-time
              tooling** (a pipeline: raw DUCET/CLDR → committed `.coll` → vendored), compiled out of
              production; version skew handled by [compatibility.md](spec/design/compatibility.md)'s
              manifest + graded verdict (read-only heap-scan / legible refusal). Removes the
              format-17 baked snapshot (a format bump). Sub-slices 2a–2d in collation.md §14.
              **(Update: 2a/2b/2c/2e have since landed — `format_version 18`, real Unicode-17 root +
              `es`; only 2d (graded verdict) is pending. See collation.md §14.)**
        - [ ] **slice 3 — host-loaded Unicode-data bundle** (design landed in collation.md, **not yet
              built**): the bare binary becomes **pure `C` / ASCII**; collation tables + Unicode casing
              are **loaded from a host-supplied `JUCD` bundle** (one shared DUCET root + per-locale
              deltas + a property/casing section, merged at load) via a privileged bytes/reader
              `db.LoadUnicodeData`, instead of being compiled into each core. The slice-2 footprint
              *tiers* become **builder-tool bundle presets** (`casing-only` / non-CJK / everything). A
              **delivery** change only — no `format_version` bump, on-disk goldens unchanged. Sub-slices
              3a–3e + the `JUCD` byte format ([collation/README.md §5](spec/collation/README.md)) in
              collation.md §14; lands with/behind the compatibility.md manifest (as 2d).
        - [ ] Further locale/feature expansion (real DUCET + curated tailorings, nondeterministic
              collations, `LIKE` under non-`C`, CLDR `shifted`, CJK tier-3 data) — **possibilities,
              not scheduled work**, collation.md §14.
- [x] **Exact `decimal`** — *the* headline type: hand-rolled sign+coefficient+scale, round-half-away
      (settles the §8 rounding hotspot), PG result scales, first parameterized + first cross-family
      promotion; finite-only (documented PG divergence); decimal in a `PRIMARY KEY`/ordered index/
      UNIQUE via the scale-independent `decimal-order-preserving` encoding (`1.5` and `1.50` index as
      one). → [decimal.md](spec/design/decimal.md), [encoding.md §2.5](spec/design/encoding.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods; `round(x,n)` and other decimal functions.
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, no tz database, `±infinity`
      first-class, timestamp PK supported. → [timestamp.md](spec/design/timestamp.md)
  - [x] **time-zone database + `AT TIME ZONE` (host-loaded `JTZ` bundle)** — ✅ LANDED (Slice 1; copies
        collation's host-load model): a host loads IANA tzdata as a **`JTZ` bundle** (manifest + per-zone
        **RFC 8536 TZif** sections + alias links) via a privileged bytes/reader **`db.LoadTimeZoneData`**;
        the bare binary carries no tz data (`UTC` + fixed `±HH:MM` offsets built-in, the `C` analogue).
        Each core has a TZif reader (`(zone, instant) → (offset, abbrev, dst)`, incl. the POSIX footer);
        the **`AT TIME ZONE` consumer** (both directions; unknown zone `22023`, non-text zone `42883`).
        **No `format_version` bump** — `timestamptz` is UTC, so plain indexes are tz-immune; the
        collation-style version-skew machinery stays **latent** until tz-derived stored keys exist
        (timezones.md §8 → compatibility.md then). Cost unit `timezone`, corpus directive
        `# load-timezone:`. → [timezones.md](spec/design/timezones.md), [tz/README.md](spec/tz/README.md).
  - [x] **the tz conversion surface (Slice 2)** — ✅ LANDED: `date_trunc(unit, src)` (2-arg ts/tstz/
        interval + 3-arg `date_trunc(unit, tstz, zone)`), `EXTRACT(field FROM src)` → `numeric` (ts/tstz/
        date/interval; field-validity matrix matches PG, `0A000`/`22023`), the cross-family `timestamp`/
        `timestamptz`/`date` **casts in a zone**, and the now-observable **session `TimeZone` slot** (the
        zone a `timestamptz` decomposes in). Session zone drives *computation*, not yet *rendering*
        (timezones.md §9.5). → [timezones.md §9](spec/design/timezones.md), [grammar.md §50](spec/design/grammar.md).
  - [ ] _further follow-on:_ `date_part` (float8 — needs `float`), `make_timestamptz`, `to_char`/
        `to_timestamp`, `age`, `EXTRACT(julian …)`; separate `time` type; **text⇄datetime casts** + the
        **session-zone rendering** of `timestamptz`; `timestamp(p)` precision typmods (timezones.md §9).
- [x] **`date`** — a calendar date (i32 days since 1970-01-01, reusing timestamp's calendar core):
      strict ISO `YYYY-MM-DD` literals (string-adapt + `DATE '…'`) with BC era + `±infinity`, a date
      `PRIMARY KEY` (key encoding = i32; on-disk type code 16, no format bump); a **strict island** —
      no compare/cast to timestamp this slice (a documented PG divergence). → [date.md](spec/design/date.md)
  - [ ] _follow-on:_ **date arithmetic** (`date ± int` → date, `date - date` → int, `date ± interval`
        → timestamp, `date + time` → timestamp); **casts** (text↔date, date↔timestamp — the latter
        unblocks cross-family `date < timestamp`); **clock-relative literals** (`today`/`tomorrow`/
        `yesterday`/`now`/`epoch`, on the entropy/clock seam); **date functions** (`make_date`,
        `EXTRACT`/`date_part`, `date_trunc`, `current_date`). → [date.md §6](spec/design/date.md)
- [x] **Typed string literals + string-literal casts (`type 'string'`)** — one generalized
      production = `CAST('string' AS type)`; literal-only coercion preserves strictness.
      → [grammar.md §36](spec/design/grammar.md)
  - [ ] _follow-on:_ runtime text→T cast on a non-literal text expression (shared with the text follow-on).
- [x] **`::` cast operator** (`expr :: type`) — desugars to the `Cast` node; binds tighter than
      unary minus; a bind-param operand takes the cast target as its type. → [grammar.md §37](spec/design/grammar.md)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, the
      engine's first timestamp arithmetic; on-disk type code 11; interval PK/index/UNIQUE/FK/GIN via
      the 16-byte `interval-span-i128` span key (span-equal values share a key).
      → [interval.md](spec/design/interval.md), [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ CAST to/from interval; ISO-8601 `P…` + SQL-standard
        input; field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `justify_*`/`EXTRACT`/`age`.
- [x] **`bytea`** — variable-width bytes, unsigned byte order, `\x`-hex literals (`22P02` on bad
      hex), on-disk type code 7; bytea PK/index/UNIQUE via the `bytea-terminated-escape` key
      encoding. → [types.md §13](spec/design/types.md), [encoding.md §2.6](spec/design/encoding.md)
  - [ ] _follow-on:_ traditional escape input (`\nnn`); bytea⇄other casts; binary functions
        (`length`, `||`, `substring`, `encode`/`decode`, `get_byte`).
- [x] **`uuid`** — fixed 16 bytes, PG-flexible input, canonical lowercase output, on-disk type code
      8; the **first non-integer `PRIMARY KEY`** (exercises `uuid-raw16` key encoding).
      → [types.md §14](spec/design/types.md)
  - [ ] _follow-on:_ uuid⇄other casts (`text ⇄ uuid`, `bytea ⇄ uuid`).
- [x] **uuid extractor functions** — `uuid_extract_version` / `uuid_extract_timestamp` (immutable);
      landed the catalog `volatility` field. → [functions.md §12](spec/design/functions.md)
- [x] **uuid generator functions** — `uuidv4()` / `uuidv7([shift])`; landed the host-injected
      entropy+clock seam (splitmix64 PRNG). → [entropy.md](spec/design/entropy.md)
- [x] **Current-time functions** — `now()` (STABLE) / `current_timestamp` (sugar) /
      `clock_timestamp()` (VOLATILE) on the clock seam. → [functions.md §12](spec/design/functions.md)
- [x] **`f32` + `f64` (IEEE 754)** — two-width promotion tower; the first types **narrowly**
      exempted from cross-core byte-identity (only transcendental *values* + render *layout*, via the
      `R` tag's tolerant compare); established the determinism framework + exception ledger; NaN
      canonicalized on store. On-disk type code 12. → [float.md](spec/design/float.md),
      [determinism.md](spec/design/determinism.md)
  - [ ] _follow-on:_ float in a PRIMARY KEY/index (`0A000`); key rule authored, unexercised.
- [ ] **`json` / `jsonb`** — optional headline feature (§1). Large surface. _(size: XL; §4)_
- [ ] **`array` type** — the **second container axis** (sibling to composite, sharing ~80% of its
      foundation): a **structural** `Type::Array(Box<Type>)` over any element type, with array
      *shape* a property of the value (PG-faithful), the compact null-bitmap value codec,
      btree-NULL element comparison (*not* composite 3VL), and `array_in`/`array_out`. **Landed:**
      S0–S5 (`format_version` 10; subscripting `a[i]` in S3; multidim values + custom lower bounds +
      slices `a[m:n]` in S5); the **AF1–AF7 function/operator surface** (`anyarray`/`anyelement`
      polymorphism, introspectors, builders, `||`/`@>`/`<@`/`&&`, `unnest`, `ANY`/`ALL`/`SOME` + the
      subquery quantifier form, `VARIADIC`); and **composite-element arrays** (AC1, the composite
      array-field CMP-ARR-FIELD, `unnest(composite[])` AF7). → [array.md](spec/design/array.md), [array-functions.md](spec/design/array-functions.md)
  - [ ] _remaining follow-ons (each its own slice + obligations):_ arrays-in-keys
        (`0A000`, encoding authored §8); runtime text→array, `array::text`, and element-wise
        array→array casts. (The subquery quantifier form `op ANY/ALL(SELECT …)` has landed — §11.6.)
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — ✅ **COMPLETE (S0–S6).** The
      **second container axis** that turns the *closed* type enum into an *open*, user-defined type
      system: `Type { Scalar | Composite(catalog-ref) }` threaded through parser/resolver/evaluator/
      codec/comparator/catalog in all three cores; `CREATE`/`DROP TYPE`, nested + recursive types, a
      storable composite **column** + recursive value codec (`format_version` 9), `ROW(…)`
      construction, field access `(expr).field`/`(expr).*`, element-wise compare/`ORDER BY`/
      DISTINCT/GROUP BY, the non-recursive all-fields `IS NULL` rule, and PG-exact `record_in`/
      `record_out`. Named composites only. → [composite.md](spec/design/composite.md)
  - [ ] _still narrowed (relaxable later):_ `INSERT … SELECT` / `UPDATE` of a composite column;
        composite `PRIMARY KEY`/index/`UNIQUE` (`0A000` — key encoding authored, unexercised);
        `DEFAULT` on a composite column; runtime non-literal text→composite + `composite::text` +
        anonymous `ROW(…)::type` casts; the nested `ROW(ROW(…),…)`-into-column constructor.

---

## Phase 4 — Relational depth + constraints

> The meaty planner/executor work and the rest of the integrity story.

- [x] **`JOIN` — multi-table FROM + `INNER`/`CROSS` + outer (`LEFT`/`RIGHT`/`FULL`)** — left-deep
      nested-loop executor, table aliases, qualified column refs, a flat-index scope resolver;
      ambiguity `42702`, dup alias `42712`; the outer NULL-extension branch + WHERE-downgrades-to-
      inner. → [grammar.md §15](spec/design/grammar.md)
  - [ ] _follow-on:_ `USING` / `NATURAL` / comma-`FROM` / `t.*`.
- [x] **Subqueries** — uncorrelated scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS`
      (plan-time folding; `21000`/`42601`); **correlated** (resolve/execute split + scope chain);
      subqueries in `UPDATE`/`DELETE`; `$N` inside a subquery (`42P18` for the lone undetermined
      case); **derived tables** `FROM (SELECT …) AS t`; a `VALUES` body `FROM (VALUES …) AS v(x)`;
      **`LATERAL`** (dependent join, SRFs implicitly lateral); and `x op ANY/ALL(SELECT …)`.
      → [grammar.md §26/§42/§44](spec/design/grammar.md), [array-functions.md §11.6](spec/design/array-functions.md)
  - [ ] _follow-on:_ a correlated `GROUP BY` / `ORDER BY` key (`0A000`, degenerate).
  - [ ] _follow-on:_ a **parenthesized-join FROM** (`FROM (a JOIN b ON …)`); a trailing **`ORDER
        BY`/`LIMIT` on a VALUES body**; **comma-`FROM`** (`FROM t, LATERAL (…)`) — until it lands,
        LATERAL is reached only through explicit `JOIN` syntax.
  - [ ] **Subqueries — remaining seams:** subqueries in an **`INSERT ... VALUES`** slot (blocked on
        VALUES holding a general expression); **row-valued** subqueries. _(size: S)_
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — a query-expression
      precedence tree (INTERSECT binds tighter), full-PG per-column type unification, NULL-safe
      multiset semantics, trailing ORDER BY by output-column name. → [grammar.md §25](spec/design/grammar.md)
  - [ ] _follow-on:_ parenthesized operands `(SELECT …) UNION …`; ORDER BY/LIMIT inside an operand;
        ORDER BY ordinals; a set op in an `INSERT … SELECT` source.
- [x] **Common table expressions (`WITH`)** — `WITH name [(cols)] AS [NOT] MATERIALIZED (query)
      [, …]`: named derived tables in FROM (forward-only visibility; PG hybrid inline/materialize
      rule, the `cte_scan_row` cost unit; `42712`/`42P01`/`42P10`). Plus **`WITH RECURSIVE`**
      (iterate-to-fixpoint working-table executor, `42P19`, cost-ceiling termination), **data-
      modifying (writable) CTEs** (one pre-statement snapshot, lexical order, all-or-nothing), and
      **nested `WITH`** inside a subquery/derived table/CTE body. → [cte.md](spec/design/cte.md), [recursive-cte.md](spec/design/recursive-cte.md), [writable-cte.md](spec/design/writable-cte.md)
  - [ ] _follow-on:_ a nested `WITH` **inheriting enclosing CTEs** (the residual visibility divergence
        above); recursive-CTE deferrals (`SEARCH`/`CYCLE`, a set-op / `FROM`-subquery recursive term,
        mutual recursion).
- [x] **Set-returning functions** — `generate_series(start, stop [, step])` in FROM position, a
      synthetic one-column relation, a new `generated_row` cost unit; integer variants (timestamp
      waits on interval composition). → [functions.md §10](spec/design/functions.md)
  - [ ] _follow-on:_ the column-alias-list `AS g(c)`. (`LATERAL` ✅ landed — an SRF is implicitly
        lateral, [grammar.md §44](spec/design/grammar.md); `unnest(array)` ✅ landed — AF3.)
- [x] **`NOT NULL`** — explicit column constraint; storing NULL → `23502`.
      → [constraints.md §1](spec/design/constraints.md)
- [x] **`DEFAULT` (literal)** — evaluated + coerced once at CREATE TABLE; landed with the INSERT
      column list + the `DEFAULT` value keyword. → [constraints.md §2](spec/design/constraints.md)
- [x] **`DEFAULT` (expression)** — non-constant `DEFAULT <expr>` (e.g. `uuidv7()`, `1 + 1`) stored
      as expression text + evaluated per row at INSERT through the entropy/clock seam; `format_version`
      8. → [constraints.md §2](spec/design/constraints.md)
  - [ ] _follow-on:_ `UPDATE ... SET x = DEFAULT` and `INSERT ... DEFAULT VALUES`.
- [x] **Composite `PRIMARY KEY`** — table-level `PRIMARY KEY (a, b, …)`; key bytes = members'
      concatenated encodings; the secondary-index catalog reshape (`format_version` 5) lifted the
      declaration-order narrowing. → [constraints.md §3](spec/design/constraints.md)
  - [ ] _follow-on:_ composite point-lookup / prefix pushdown (a composite-PK table full-scans today
        — an optimization slice with its NoREC obligation).
- [x] **`CHECK` constraints** — column- + table-level `[CONSTRAINT name] CHECK (expr)`, enforced
      per candidate row inside the two-phase pass (`23514`), PG auto-naming; persisted as
      `(name, expression-text)` under `format_version` 4. → [constraints.md §4](spec/design/constraints.md)
- [x] **`UNIQUE` constraints + unique indexes** — column-/table-level `UNIQUE` and `CREATE UNIQUE
      INDEX`; a UNIQUE constraint **is** its backing unique index; NULLS-distinct enforcement;
      `format_version` 6 (per-index flags byte). Unlocks `ON CONFLICT`.
      → [constraints.md §5](spec/design/constraints.md), [indexes.md §8](spec/design/indexes.md)
- [x] **`FOREIGN KEY` constraints** — column-level `REFERENCES` + table-level `[CONSTRAINT name]
      FOREIGN KEY (cols) REFERENCES parent (cols) [ON DELETE/UPDATE …]`; composite + self-reference;
      referenced columns must be the parent PK or a UNIQUE set (`42830`), same-type pairing (`42804`,
      stricter than PG); MATCH SIMPLE; enforced at four write sites (`23503`) in the two-phase pass,
      batch-end-state-aware; `DROP TABLE` of a referenced table is `2BP01`; persisted under
      `format_version` **11**. → [constraints.md §6](spec/design/constraints.md), [grammar.md §43](spec/design/grammar.md)
  - [ ] _follow-on:_ the referential **actions** `ON DELETE/UPDATE CASCADE | SET NULL | SET DEFAULT`
        (parse but `0A000` today — they write the child during a parent mutation); `MATCH FULL`;
        a **backing index** on the child FK columns (the parent-side check full-scans children today);
        FK type pairing relaxed to PG's comparable-types; `ALTER TABLE … ADD/DROP CONSTRAINT`.
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — non-unique on-disk B-trees of
      empty-payload records, maintained in the two-phase pass; the planner index-bounds a SELECT
      base scan on a first-column equality; `format_version` 5 catalog reshape; DROP code `42809`.
      → [indexes.md](spec/design/indexes.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ index ranges / multi-column prefixes;
        index scans for UPDATE/DELETE (keep PK pushdown today); LIMIT-streaming combination;
        the lone not-yet-key-encodable index type (`float` keys — boolean, text, bytea, decimal, and
        interval have since landed); expression/ordered/partial keys; `IF NOT EXISTS`.
- [ ] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* beside the
      ordered B-tree, via a type-generic operator-class seam. **Landed (G0–G2 + follow-ons):** the
      **`array_ops`** opclass over array columns (one entry per distinct non-NULL element, empty
      payload; `format_version` 12's `index_kind` byte; a `gin_entry` cost unit), accelerating
      `@>`/`&&`/`= ANY(col)`/array `=` for SELECT **and** GIN-bounded UPDATE/DELETE, over integer
      **and** the other fixed-width key-encodable element types (`uuid[]`/`date[]`/`timestamp[]`/
      `timestamptz[]`/`boolean[]`). → [gin.md](spec/design/gin.md)
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the
        index recording empty/NULL-array rows) / `IN` over a scalar list; the **remaining** element
        types — the VARIABLE-width keyables (`text[]`, `bytea[]`, `decimal[]`) need GIN term framing
        (a term carries no length/terminator), and `float[]` needs its key encoding to lift first;
        `interval[]` is now UNBLOCKED (its fixed-width 16-byte span key landed, encoding.md §2.10) but
        its GIN element support is its own slice — plus composite-element arrays; multi-column GIN; correlated / array-column query operands; the
        **ordered-index** equality bound for UPDATE/DELETE (mutations use PK+GIN but not the ordered
        index yet); the LIMIT-streaming combination; posting-list run compression; the **`jsonb_ops`**
        opclass (the lossy-recheck path the seam already seats) and a future object/document opclass.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <select_items>` projecting affected
      rows (INSERT stored / UPDATE new / DELETE old), evaluated after validation before any write;
      the PG-18 `old.`/`new.` row-version qualifiers landed as a follow-on.
      → [grammar.md §32](spec/design/grammar.md)
  - [ ] _follow-on:_ the `WITH (OLD AS o, NEW AS n)` aliasing form; `old.*`/`new.*`.
- [x] **Sequences** (`CREATE SEQUENCE` / `nextval` / `currval`) — ✅ **landed (S0–S6).** A
      PostgreSQL sequence object as a third catalog-object kind: a named, persisted, monotonic i64
      generator (`entry_kind = 2`, `format_version` 12, a `sequence_advance` cost unit), with
      `nextval`/`currval`/`setval`/`lastval`, `serial`/`bigserial`/`smallserial` + `GENERATED AS
      IDENTITY` owned-sequence columns (`format_version` 14/15), the `AS {smallint|integer|bigint}`
      data type, and the `ALTER SEQUENCE` option set + `RENAME TO`. **The defining decision —
      `nextval` is TRANSACTIONAL** (rolls back with the txn), a deliberate PG divergence mandated by
      [determinism.md §5](spec/design/determinism.md). → [sequences.md](spec/design/sequences.md)
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET …
      [WHERE …] }`: a candidate row that would violate a UNIQUE/PK constraint takes the conflict
      action instead of `23505`; the proposed row is the qualifier-only `excluded` pseudo-relation;
      arbiter = a column SET or `ON CONSTRAINT name` (`42P10`/`42704`/`42601`); two-phase /
      all-or-nothing; RETURNING projects inserted + updated rows. No new error code, no format change.
      → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT` (with the `UPDATE` `SET = DEFAULT` follow-on);
        `INSERT INTO t AS alias` (the existing row is referenced by the table name today); the
        partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing
        the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed,
        but the conflict-path re-key (the existing row moves) is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [x] **Relax the UPDATE narrowings** — assigning a `PRIMARY KEY` column now **re-keys** the row:
      its storage key is recomputed and the row moves (secondary-index entries follow). The new
      keys are validated against the statement's end state, so a collision traps `23505` and a
      re-key that strands a child (incl. a self-reference) traps `23503`; an end-state-valid
      swap/cascade succeeds where PG fails the per-row transient (the `UNIQUE` end-state
      divergence, constraints.md §6.5). Two-pass phase 2 (vacate old keys, then place at new) so a
      key chain/swap never transiently collides. All three cores; no `format_version` bump; the DO
      UPDATE conflict-path equivalent remains a deferred `0A000` follow-on (above). (§11 step 6.)
- [ ] **Temporary tables** — `CREATE [SHARED] [TEMP|TEMPORARY] TABLE` (+ `DROP`): relations that make
      **zero writes to the database file** (held outside the serialized `Snapshot`, so no
      `format_version` bump), bounded by a deterministic storage budget so they keep the
      untrusted-SQL guarantee (§13). Two kinds: **session-local** (private to the session, no writer
      gate, usable even by a read-only session) and **database-wide shared** (visible across sessions,
      transactional + single-writer-gated, published at commit with a no-fsync second-root swap).
      Namespace **precludes overlaps** (`42P07`; a PG divergence — no `pg_temp` shadowing). New code
      **`54P03 temp_storage_limit_exceeded`** + settings `temp_buffers` (session) / `shared_temp_mem`
      (global). **`allow_ddl` splits by relation kind** into `allow_ddl` (persistent) /
      `allow_temp_ddl` / `allow_shared_temp_ddl` (the two new gates default to `allow_ddl`'s value;
      untrusted-scratch = `allow_ddl off` + `allow_temp_ddl on`). Phased: **(1) session-local,
      memory-only + `allow_temp_ddl`** → **(2) shared + `allow_shared_temp_ddl`** (needs the
      concurrency schedule format) → **(3) spill-to-disk** (the resident→paged flip onto a temp
      `BlockStore`; the seam is already open, CLAUDE.md §9). FK touching a temp table + `ON COMMIT
      DELETE ROWS`/`DROP` deferred `0A000`. → [temp-tables.md](spec/design/temp-tables.md)
      _(size: L; deps: session model (done), storage seam (done); spill in slice 3)_
  - [ ] _follow-on:_ `ON COMMIT DELETE ROWS`/`DROP`; `IF NOT EXISTS`; `CREATE TEMP TABLE … AS SELECT`;
        FKs among same-kind temp tables; temporary views. → [temp-tables.md §14](spec/design/temp-tables.md)

---

## Phase 5 — Transactions & the §3 commit model

> ✅ **Phase 5 is landed (P5.0–P5.3, all three cores).** The model is immutable **`Snapshot`**s +
> a writer's **working root** (unifying staging area, read snapshot, and pending set), over a
> persistent (copy-on-write) ordered B-tree (decision **B1**, the in-memory precursor of the
> Phase-6 on-disk B-tree). jed adopts **PostgreSQL autocommit** and **decouples the commit
> boundary from durability** via a `synchronous` setting. Ships fully durable + §3-correct on
> whole-image commit; only on-disk *efficiency* was deferred to Phase 6. The oldest-live-txid
> **watermark** is the free-list gate Phase 6 consults. → [transactions.md](spec/design/transactions.md)

- [x] **P5.0 — transaction model spec** — authored transactions.md; reconciled storage.md / api.md /
      CLAUDE.md §9; registered class-25 errors `25001`/`25006`/`25P02`.
- [x] **P5.1 — persistent ordered map + the snapshot refactor** — `pmap` CoW B-tree (O(1)
      structurally-shared clone), `TableStore` wrap, autocommit through the single `persist`
      chokepoint (rollback-on-error), `close` no longer drops committed work.
- [x] **P5.2 — explicit transactions** — SQL `BEGIN`/`COMMIT`/`ROLLBACK` (+ `READ ONLY|WRITE`) and
      the `Transaction` API (`db.begin`/`view`/`update`); a current-transaction state machine, class-25
      errors, failed-block abort + commit-as-rollback. → [grammar.md §27](spec/design/grammar.md),
      [api.md §6](spec/design/api.md)
- [x] **P5.3 — reader/writer concurrency + the watermark** — immutable `Snapshot` + `Database{committed,
      tx}` split (P5.3a); a `SharedDb` handle realizing concurrent readers + a single writer with the
      live-reader registry (`oldest_live_txid`, the Phase-6 free-list gate) (P5.3b). Rust/Go give true
      OS-thread parallelism, TS snapshot isolation; tested per-core (Go under `-race`).
      → [transactions.md §8/§10](spec/design/transactions.md), [api.md §2.5](spec/design/api.md)
- [x] **P5.4 — cross-core concurrency conformance** — the `# format: concurrency` schedule (an
      explicit total order over named sessions on one `SharedDb`, deterministic by commit order +
      pin-points): **Layer 1** the schedule format + caps `txn.shared`/`txn.read_handle`/`txn.watermark`;
      **Layer 2** the write-gate `blocks` annotation (`txn.gate_blocking`); **Layer 3** the
      `stress/*.stress.toml` parallelism-stress format (`rake stress`, outside `rake ci`, invariant-
      + checksum-checked). Stepped-sequential everywhere; Go + Rust also run stepped-threaded under
      `-race`. → [concurrency-testing.md](spec/design/concurrency-testing.md)

---

## Phase 6 — Storage maturation (§9)

> Can lag the feature work until write volume makes whole-image rewrites costly.
>
> **TB-scale non-foreclosure (CLAUDE.md §9):** these items are also the path to a
> **larger-than-RAM file that does not fall over**. RAM-sized is the dominant case but not a
> hard limit — present work must not foreclose >>RAM operation (no full-residency assumption
> above the storage seam; no operator that requires its whole input/output in RAM).

- [x] **P6.1 — incremental COW commit = page-backed B-tree** _(merged "incremental COW commit" +
      "B-tree interior pages")_ — whole-image serialize replaced by dirty-page-only writes + meta-slot
      root swap; `format_version` 2 byte contract (page-backed B-tree, size-driven fan-out,
      delete-rebalance), 15 goldens byte-exact `rust==go==ts==ruby`; dropped pages leak (reclamation
      is P6.2). → [storage.md §4/§6](spec/design/storage.md)
- [x] **P6.2 — free-list / page reclamation** _(reconstruct-on-open form)_ — the free-list is rebuilt
      on open (`[2, page_count)` minus reachable pages) and the commit allocator reuses it lowest-index
      first; torn-write-safe, gated on the oldest-live watermark. Byte format unchanged.
      → [transactions.md §8](spec/design/transactions.md)
  - [ ] _follow-on (where the watermark does real work):_ continuous *within-session* reclamation
        (return a commit's orphans immediately, paired with file-backed reader sharing); on-disk
        free-list persistence (claim meta offset 28 to skip the open-time reachable-set walk).
- [ ] **File compaction / shrink (return space to the OS)** — ⏳ **approach decided
      (`to_image`-based whole-image compaction), not built.** The free-list (P6.2) recycles dead
      space for *jed*, but `page_count` is a monotonic high-water (+ pager.md §7 preallocation
      slack), so the file is **grow-only** (SQLite's and PG's default too). The decided shrink
      mechanism is a **host-invoked compaction that re-serializes the committed snapshot through the
      from-scratch `to_image` serializer** into a fresh file and atomically swaps it in (the `create`
      temp-file + fsync + rename recipe), then re-adopts the pager on the new minimal file. One pass
      reclaims **all** dead space + defragments (the SQLite `VACUUM` / PG `VACUUM FULL` flavor) and is
      crash-safe for free. **Explicit / host-invoked, not automatic-per-commit** (per-commit
      truncation would fight §7 preallocation), gated on the reader-liveness watermark. Needs nothing
      new at the storage seam. A lighter **in-place trailing-free truncation** (the PG-plain-`VACUUM` /
      SQLite-`incremental_vacuum` flavor) stays open as a cheaper *partial* complement. Recorded in
      [storage.md §6](spec/design/storage.md). _(size: M–L; deps: P6.2; §9)_
- [x] **P6.3 — `page_read` cost unit + corpus cost re-baseline** — a distinct logical `page_read`
      unit (per B-tree node visited, structural node count — a future buffer pool stays invisible);
      re-baselined atomically across all three cores, byte format untouched.
      → [cost.md §3](spec/design/cost.md), [schedule.toml](spec/cost/schedule.toml)
- [x] **P6.4 — buffer pool / demand paging** — the resident set is now a bounded cache of **leaf**
      pages with CLOCK eviction (the interior skeleton stays resident, so `page_read` stays
      structural + cost byte-identical to P6.3). Handle-level `cache_pages` budget (default 1024).
      Sub-slices P6.4a (pager seam) / P6.4b (lazy leaves + bounded pool) / P6.4c (budget config) all
      landed. → [pager.md](spec/design/pager.md), [api.md §2.1](spec/design/api.md)
- [ ] **Streaming + spill-to-disk operators** — bound blocking operators (`ORDER BY`, hash
      `JOIN`, `GROUP BY`/aggregate, `DISTINCT`) by a memory budget and **spill to disk** when
      exceeded (external merge sort, grace hash join), so a query over larger-than-RAM data
      never materializes its whole input/output in memory. Designed in
      [spill.md](spec/design/spill.md). _(size: XL; deps: paged storage; §9/§13)_
  - [x] **External merge sort for `ORDER BY`** — a `Sorter` bounded by `work_mem` (default 256 MiB)
        spills sorted runs + k-way merges, reproducing the in-memory stable sort byte-for-byte;
        result- and cost-invariant; the single-table path fuses scan→filter→Sorter.
  - [ ] **Spilling hash aggregate / `DISTINCT` / hash JOIN** — the remaining blocking operators
        (spill.md §7). Each needs a *different* algorithm: a partitioned (grace) hash that preserves
        first-occurrence order for aggregate/DISTINCT, and — for hash JOIN — a hash-join operator
        first (jed joins are nested-loop today), then grace-hash spill to bound the build side.
        _(size: L–XL each)_
- [ ] **Bench-driven perf follow-ons** — the `perf-point-lookup` branch (2026-06-13) took
      `point_lookup_pk` past same-language PG clients in all 3 cores (rust 5.4µs / go 6.6µs /
      ts 17.3µs vs PG 10.2/12.6/18.4) via the 256 MiB pool default, binary-searched descent
      windows, fused single-descent scans, and TS codec hot paths; `secondary_lookup` fell
      ~93% to PG parity. The measured gaps that remain, with their diagnoses:
      - **Rust CoW insert deep-clone** — `node_insert` rebuilds a path node with `Vec::clone`,
        deep-copying every key (`Vec<Vec<u8>>`) and row, where Go's `[][]byte` copy is
        pointer-shallow — why `insert_rollback` is rust 21.6ms vs go 10.3ms. Fix: share entry
        storage (`Arc<[u8]>` keys / `Arc`-shared rows). Rust-only, no byte or cost change. _(size: M)_
      - **ORDER BY + LIMIT top-k** — `order_by_limit` is 0.76–1.6s vs PG ~20ms: the executor
        full-sorts all 1M rows before slicing. A bounded top-k selection (heap of LIMIT+OFFSET,
        index-stable tie-break) cuts the sort to ~scan cost. Rows + cost unchanged (sort unmetered).
        _(size: M; ×3 cores)_
      - **Full-scan materialization** — `full_scan_agg` is 143–281ms vs PG ~13ms: the eager path
        clones every row into a materialized buffer before aggregating. Streaming aggregation over
        the scan visitor is the contained first step; the full fix is the spill item above. _(size: M–L)_
      - [x] **Durable-commit sync cost** — pager preallocates file growth in 1 MiB chunks +
            `fsync`→`fdatasync`, so steady-state commits overwrite already-allocated space metadata-
            free: ~9.0ms → ~2.5–3.1ms p50 (~2.7×), identical cross-core checksums. → [pager.md §7](spec/design/pager.md)
- [x] **Large values — overflow pages + compression (TOAST-equivalent)** — large `text`/`bytea`/
      `decimal`/future `json` pushed out-of-line onto overflow-page chains (Slice A, `format_version`
      3), optionally LZ4-compressed first via a deterministic hand-rolled block codec (Slice B,
      no third-party dependency — a library fails §8 byte-identity). Plus the touched-column cost
      contract + physical lazy read-on-touch storage. Unblocked decimal's raised cap and `json`/`array`.
      → [large-values.md](spec/design/large-values.md), [lz4.md](spec/fileformat/lz4.md)
  - [ ] _follow-on:_ chain sharing on rewrite (let a rewritten record keep an unchanged value's
        existing chain — a byte-layout change, lands in all cores + incremental tests together).
- [x] **Crash-recovery hardening** — a pager fault-injection seam (armed at `BodyWrite`/`MetaWrite`/
      `Sync`, optional torn page) + a cross-core recovery matrix proving a crash *anywhere* recovers
      to a valid snapshot, never corrupt; free-list reconstruction stays correct. WAL stays deferred
      (COW + root-swap gives atomicity without one). → [storage.md §7](spec/design/storage.md)

---

## Phase 7 — Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters have
> landed; the browser/OPFS host remains. Parallelizable with most feature work.

- [x] **Formal public API** — `create`/`open`, crash-safe explicit `commit` / `close`, `prepare`,
      execute, a `Rows` cursor, structured errors (+ class-58 host codes); same shape across all
      three cores. → [api.md](spec/design/api.md)
- [x] **Parameterized queries (`$1`)** end-to-end — lexed/parsed, context-typed at resolve (`42P18`
      if indeterminate), bound two-phase before any scan; tested per-core (corpus stays literal-only).
- [ ] **Storage hosts** — formal interface authored in [hosts.md](spec/design/hosts.md): the
      five-method `BlockStore` byte device, the host catalog, the decoration layering (encryption
      codec above the seam, replication tee below). Node `fs` host built; Rust/Go inline
      `std::fs`/`os` in the per-core `Pager`. **Landed:** the `FileBlockStore` extraction and the
      **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity
      vs goldens, gated Playwright e2e). Deferred: OPFS disk-spill, the e2e in CI. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [x] **Cost ceiling (`max_cost`) + deterministic abort** — a handle `max_cost` aborts a statement
      with `54P01` the instant accrued cost reaches it (`Meter::guard()` at the unbounded-work
      points; the `# max_cost:` directive). Plus a fixed `MAX_EXPR_DEPTH = 256` parser nesting bound
      (`54001 statement_too_complex`) closing the native-stack-overflow gap the cost ceiling can't
      catch. → [cost.md §6/§7](spec/design/cost.md)
- [x] **The `jed` CLI** — a full-screen TUI client (Rust + ratatui/crossterm/tui-textarea, the
      §14-approved deps) + a plain script mode (`-c`/`-f`/stdin; aligned/csv/json). A host program,
      not a core. → [cli.md](spec/design/cli.md)
- [x] **Affected-row counts in `Outcome`** — DML without RETURNING reports rows touched (PG command
      tags), an additive `Outcome` field in all 3 cores. → [api.md §4](spec/design/api.md)
- [x] **CLI follow-ons** — editor autocomplete + syntax highlighting, CSV import/export, `--dump`
      SQL export, `-o` redirection, `box`/`markdown` formats, `--readonly` open mode. → [cli.md §8](spec/design/cli.md)
- [ ] **Sessions — the configured host context** — un-fuse `Database` (storage identity) from a
      first-class **`Session`** (the configured, capability-bearing context a host runs statements
      through). Spec: → [session.md](spec/design/session.md). **Landed (S1–S4):** the `Session` type +
      the one stateful default session + explicit tx state machine (S1); the `split_statements`
      iterator + `execute_script` (S2); the GRANT/REVOKE privilege envelope + `allow_ddl` (`42501`,
      S3); the `lifetime_max_cost` cumulative budget (`54P02`, S4).
  - [ ] **S5 — session variables (v1)** — a string→string GUC map, host get/set + `current_setting()`
        read; namespaced custom vars; `# set:` directive. (`SET LOCAL` / full SQL `SET`/`SHOW` /
        `set_config()` deferred.) Capability `session.variables`. _(size: M; §6.1)_
  - [x] **S6 — session time zone slot** — ✅ LANDED with the tz conversion slice: the built-in
        `time_zone` var (default **`UTC`**; `UTC` + fixed offsets + **named loaded zones**, else `22023`),
        injected not OS-read (determinism, §6); the `# timezone:` directive; host-API set/validate. The
        consumers (`date_trunc`/`EXTRACT`/cross-family casts) landed too (timezones.md §9). Capability
        `session.timezone`. _(§6.2)_
- [ ] **(Open question, not scheduled)** low-level direct access API beneath SQL
      (`getValue("table", key)`) — keep the seam open, don't build yet (§9). _(size: —)_

---

## Phase 8 — Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Some pairs with earlier phases.

- [ ] **Differential-testing harness** vs the PostgreSQL oracle to bootstrap corpus
      cheaply (§7). **PARTIAL** — the **live-`db` oracle-import** tool is built
      (`scripts/oracle_import.rb`; `rake corpus:import/check`; override ledger
      `spec/conformance/oracle_overrides.toml`; conformance.md §5) and needs no §12 provisioning.
      *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated**
      reference provisioning §12 — never auto-provision). **SQLite is deliberately not an oracle**
      (CLAUDE.md §7); mining its sqllogictest corpus for *query shapes* (answers from PG) is the
      only oracle-adjacent use. _(size: M remaining; §7)_
- [ ] **SQLancer-style metamorphic / generative testing** — finds logic bugs by synthesizing
      queries with known-correct answers. **PARTIAL** — built so far (`scripts/norec_gen.rb`;
      `rake corpus:norec_sweep`, in `rake ci`; conformance.md §8): the **NoREC** slice (pushdown
      predicate vs non-optimizable rewrite must agree — scenarios pushdown / limit / join /
      correlated / index), the **TLP** slice (ternary-logic partitioning, an independent oracle for
      3-valued NULL logic), and an automatic **reducer** (`scripts/reduce.rb`; ddmin over records).
      *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator),
      `SUM`/`MIN`/`MAX`/`AVG` + `GROUP BY` TLP (blocked on `COALESCE`/`LEAST`/`GREATEST`), and
      **broader NoREC relations** (see the growth obligation below). _(size: M remaining; §7)_
- [x] **Result-type assertion directive** — the `# types:` directive asserts each result column's
      precise resolved type (`i16` vs `i32`) beyond the render tag; `numeric(p,s)` typmod
      granularity stays deferred. → [conformance.md §7](spec/design/conformance.md)
- [ ] **Corpus growth** — keep adding `.test` coverage as each feature lands (ongoing). Two
      **standing obligations** when a feature lands (conformance.md §5/§8): (a) on the
      PG-comparable surface, run `rake corpus:check` on the new `.test` and register any
      intentional divergence in the override ledger; (b) **when you add a query optimization or a
      new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does
      **not** discover new optimizations, and adding *seeds* does not add coverage. NoREC covers
      point-lookup + range pushdown, `LIMIT` short-circuit, JOIN base-table pk pushdown, and
      correlated-subquery pushdown today; future index/DISTINCT/aggregate pushdown are **not yet** covered.
- [ ] **Benchmark backfill** — grow `bench/corpus` beyond the v1 set
      (spec/design/benchmarks.md §11; built: cross-core + cross-engine wall-clock harness,
      `rake bench:setup/run/report`, six benchmarks over 10k/1M-row datasets): a join benchmark
      (needs a second dataset table → `generator_version` bump), GROUP BY aggregate,
      UPDATE/DELETE throughput, miss-heavy point lookups, text/large-value-heavy rows (the
      overflow + LZ4 path), `SharedDb` concurrent-reader throughput (once file-backed),
      cold-open time, durable-commit batch-size sweep. **Standing obligation** (CLAUDE.md §10):
      a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected
      benchmarks before/after and reports both numbers. _(size: M, ongoing; §10)_

---

## Phase 9 — Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, spec/design/cores.md).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). **Two pivots** decide it (cores.md §2.1–§2.2): (1) host-function hotness —
> hot-path per-row favors native, coarse favors wrap; (2) parallelism — wrapping Rust hands
> every host Rayon-grade intra-query parallelism free (and dodges Swift's ARC-contention),
> while native is strong for C#/Java (GC-cheap sharing) and weak for Swift. Wrapping the safe
> Rust core is a **first-class** choice here, not an exception. Any native core still passes the
> full conformance contract (§7/§8); a wrap inherits it from Rust.

- [ ] **C#** core — **lean native** (value-type generics, `Span<T>`, NativeAOT → near-Rust
      speed *and* a clean pure-managed NuGet package; in-process host functions). Strongest
      native candidate. _(size: XL native / L wrap; §2)_
- [ ] **Swift** core — **lean wrap** (UniFFI + XCFramework over the safe Rust core: Rust
      speed, well-trodden Apple packaging, untrusted-query safety preserved §13). Go native
      only if hot-path per-row host functions make the FFI upcall tax dominant.
      _(size: L wrap / XL native; §2/§13)_
- [ ] **Java** core — **conflicted**: wrap for performance (pre-Valhalla boxing + JIT warmup
      hurt a native core), native for clean pure-JAR packaging + in-process host functions
      (no JNI/upcall tax). Decide at scheduling time; Valhalla shifts it toward native.
      _(size: XL native / L wrap; §2)_
- [x] **Runtime function registry — the §5 dispatch foundation** — resolution for built-in named
      scalar functions + aggregates is now data-driven over the generated catalog tables (one
      `(name, arg_families)` lookup); the per-row kernel still reached by id, hand-written per core.
      → [extensibility.md §5](spec/design/extensibility.md)
  - [ ] _follow-on:_ built-in type-vtable dogfood (Fork A) and host registration into the table.
- [ ] **Design the host-function API vectorized/batched** up front — the single decision
      that keeps wrapping viable for any of the above (amortizes the per-row FFI upcall).
      **Sits on the runtime function registry above** — host functions register into the same
      `(name, arg_families)` table; a host name colliding with a built-in is rejected (propose
      `42723`). _(size: M; §2, cross-cutting)_
- [ ] **Host-defined functions must contribute to the cost system** — a hard requirement on
      the host-function API above, not an optional extra. A host function is otherwise
      **opaque to the meter** (its code does not route through `Meter::charge`), which breaks
      two contracts at once: the untrusted-query bound (§13 — an unmetered call can burn
      unbounded CPU past `max_cost`) and the **cross-core cost identity** (§8 — a wrapped core
      and a native core must compute the *same* cost for the same call). So the registration
      API **must** carry a cost-contribution contract. Design space (decide when scheduled;
      recorded in cost.md §6):
        - **Declared static weight** — a per-function cost in its registration (generalizing
          the reserved `cost` field in `functions/catalog.toml`): simplest, charged once per call.
        - **Declared cost-as-a-function-of-arguments** — the host supplies a *pure, deterministic*
          cost over argument values/sizes (the `decimal_work` / `value_compress` model), charged
          **up front and guarded before** the call runs.
        - **A metering callback** — the host receives a narrow `charge(n)` handle into the
          `Meter`, enabling a **chunk-boundary mid-call abort**. Must be deterministic + cross-core
          identical (no wall-clock, no allocation/iteration-order dependence — §10).
      A host that declines all three can be admitted only on a handle with `max_cost = 0`
      (unlimited) — i.e. **not** the untrusted-query surface (§13). _(size: M; §2/§13)_

---

## Ordering rationale & open tensions (for iteration)

- **Why Phase 1 first:** two canonical spec dirs (`grammar/`, `functions/`) were still
  empty, and a general expression evaluator is the prerequisite for almost everything in
  Phases 2 & 4. Cheap to do, unblocks the most.
- **Why the type system (Phase 3) is its own phase, not earlier:** it's *the product*, but
  most type work depends on the expression/operator substrate from Phase 1, and `decimal`
  (XL) shouldn't gate the SQL-shape features in Phase 2.
- **Resolved tensions:** `NOT NULL`/`DEFAULT` pulled into Phase 4 (fundamental + easy);
  `JOIN`s done for `INNER`/`CROSS`/outer; transactions (Phase 5) placed before storage
  maturation because the staging buffer couples with Phase 6's COW commit.

---

## Maybes / distant ideas (keep the door open — do NOT schedule)

> Not backlog. Architectural doors to **leave open**, not walk through now. The §9 rule —
> SQL is the primary surface and everything must be reachable through it, but it need not be
> the *only* access path — is read **broadly** here. Nothing below is a commitment; the only
> requirement is that nearer-term work not quietly foreclose these.

- **Alternative access paths beyond low-level direct reads.** §9 already keeps a sub-SQL
  `getValue("table", key)` seam open. Read that intent broadly: keep the architecture from
  foreclosing *entirely different* surfaces over the same storage + type core.
- **Other query languages.** SQL is clunky; the core (typed values, order-preserving keys,
  relational storage) need not be SQL-only. A graph query language, a document/dataframe
  surface, etc., could one day sit *beside* SQL over the same engine. Very distant — just
  don't design anything that makes it impossible.
- **Graph / vector workloads.** Growing toward graph traversal or vector-similarity search.
  §9 already flags alternative physical layouts as open (column-oriented, key-value); a
  vector index would be another. Speculative — noted so the seam stays open.
- **Encryption at rest (file-level).** Whole-file or per-page **encryption** is a door to
  keep open, not a scheduled feature (CLAUDE.md §9, storage.md §6); **designed in
  [spec/design/encryption.md](spec/design/encryption.md)**. The insertion point is a page codec
  **in the core above the block seam** (not a per-host duty); a standardized AEAD under a
  **deterministic `(page_index, txid)` nonce** keeps the §8 cross-core byte-identity, and the
  auth tag closes the tamper gap the `format_version` 7 CRC leaves open. Crypto comes from a
  **vetted library, never hand-rolled** (§14 — the build gate; pure-Go availability binds the Go
  core). The only present requirement is non-foreclosure (don't assume page bytes are
  plaintext-comparable on disk) — already satisfied.
- **Replication.** ✅ **Architecture decided (block-shipping, no WAL), not built** — designed in
  [spec/design/replication.md](spec/design/replication.md). Ship the **per-commit page-delta**
  (the dirty pages + meta swap the commit already produces, storage.md §4) in `txid` order, as a
  tee at the block seam. No WAL: copy-on-write + the root swap already give atomicity *and*
  lock-free concurrency, and the block-delta inherits the §8 byte-identity + the §4 atomic-apply
  recipe. The tee sits **below** the encryption codec → **keyless** backup replicas. Trade:
  write-amplification. A **logical** changeset stream (compact wire, heterogeneous consumers) is
  a separate higher-layer door at the row-mutation seam — not foreclosed, not scheduled.
