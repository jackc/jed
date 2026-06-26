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
- [x] **Author the function / operator catalog** — operator result types + NULL behavior as data,
      a family-based schema + coherence checker. → [catalog.toml](spec/functions/catalog.toml),
      [functions.md](spec/design/functions.md)
- [x] **Codegen "middle path"** — catalog → per-language operator descriptor tables (data only;
      parser/executor/evaluator stay hand-written), drift-gated by `rake verify`.
      → [gen_catalog.rb](scripts/gen_catalog.rb), [codegen.md](spec/design/codegen.md)
  - [ ] _follow-on:_ extend the generator to types/errors.
    - [x] **errors** — `SqlState` enum + code mapping + `ERRORS` table generated per core from
          [registry.toml](spec/errors/registry.toml), drift-gated by `rake verify`.
          → [gen_errors.rb](scripts/gen_errors.rb)
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
      storage reads, a data-defined unit schedule, the `# cost:` directive; ceiling+abort (`54P01`)
      and a real `page_read` unit have since landed. → [cost.md](spec/design/cost.md),
      [schedule.toml](spec/cost/schedule.toml) _(§13)_
  - [x] _follow-on:_ per-operator `cost` weights — the optional `cost` field in
        [catalog.toml](spec/functions/catalog.toml) is codegen'd into `OperatorDesc`; no built-in
        overrides it, so cost is unchanged. → [cost.md §3](spec/design/cost.md)
    - [x] **`varlen_compare` unit** — text/bytea comparison work scales with the shorter operand's
          length, the `decimal_work` analog. → [cost.md §3](spec/design/cost.md)

---

## Phase 2 — Make it feel like SQL (core query/DML completeness)

> Builds directly on the Phase 1 expression substrate.

- [x] **Select-list expressions + `*` + column aliases (`AS`)** — output column naming as a
      cross-core contract (the `# names:` directive). → [grammar.md §8](spec/design/grammar.md)
- [x] **`LIMIT` / `OFFSET`** — either order, non-negative integer literal (negative →
      `2201W`/`2201X`), applied after ORDER BY before projection. → [grammar.md §9](spec/design/grammar.md)
- [x] **Richer `ORDER BY`** — multiple keys, per-key `ASC`/`DESC`, `NULLS FIRST|LAST` (PG
      NULL-largest default). → [grammar.md §10](spec/design/grammar.md)
  - [x] **Output-column ordinal** (`ORDER BY 1`) — 1-based select-list position incl. the set-op
        `ORDER BY`; out of range `42P10` (`query.order_by_ordinal`). → [grammar.md §10](spec/design/grammar.md)
  - [x] **General-expression sort keys** (`ORDER BY a + 1`, `ORDER BY sum(b)`) — evaluated per row
        and sorted by the computed value; DISTINCT requires the key to match a select-list expression
        (`query.order_by_expr`). → [grammar.md §10](spec/design/grammar.md)
  - [x] **Output-alias sort keys** (`SELECT a + b AS s ... ORDER BY s`) — a bare name resolves an
        OUTPUT column before an input column (PG's SQL92 rule, the opposite of `GROUP BY`)
        (`query.order_by_alias`). → [grammar.md §10](spec/design/grammar.md)
  - [x] **General-expression `WITHIN GROUP` order key** — the ordered-set-aggregate order key may be
        any expression, sorted by the computed value (`query.within_group_expr`).
        → [aggregates.md §13](spec/design/aggregates.md)
  - [x] **Correlated `ORDER BY` key** — an `ORDER BY` inside a subquery referencing an enclosing-query
        column, materialized per outer row (`query.order_by_correlated`). → [grammar.md §10](spec/design/grammar.md)
  - [x] **Window function / `GROUPING()` inside an `ORDER BY` key** — the key resolves in the same
        window / group context as the projection (`query.order_by_window`, `query.order_by_grouping`).
        → [grammar.md §10](spec/design/grammar.md)
  - [x] **Expression key in a grouped+window query** — resolved in the grouped+window context and
        materialized after the window stage (`query.order_by_grouped_window`). → [grammar.md §10](spec/design/grammar.md)
    _(Two remaining `0A000`s are deliberate non-features: a general-expression key in a
    **set-operation** `ORDER BY` (PG itself rejects it `0A000`), and **GROUPING SETS + window functions**.)_
- [x] **`ORDER BY` satisfied by primary-key scan order** — a single-table non-aggregate
      non-`DISTINCT` `SELECT` whose `ORDER BY` is an `ASC` prefix of the PK elides the sort and
      streams the scan; with a `LIMIT` it short-circuits a top-N (`query.order_by_pk_scan`).
      → [cost.md §3](spec/design/cost.md), [grammar.md §10](spec/design/grammar.md)
  - [x] _follow-on:_ **`DESC` (reverse scan)** — an `ORDER BY` over the **full PK** all-`DESC` is
        served by a reverse tree walk; with a `LIMIT` it short-circuits a top-N from the high end.
  - [x] _follow-on:_ **secondary-index order** — a B-tree index whose columns satisfy the `ORDER BY`
        (with a `LIMIT`) is walked in key order + point-look-up per row, a top-N
        (`query.order_by_index_scan`).
  - [x] _follow-on:_ **`DISTINCT`** — a `DISTINCT` whose `ORDER BY` is PK-scan-satisfied dedups
        **streaming** in scan order, so with a `LIMIT` it short-circuits a top-N.
  - [x] _follow-on:_ **multi-table joins** — a two-table INNER/CROSS join whose `ORDER BY` is a prefix
        of the OUTER relation's PK (with a `LIMIT`) is served by the nested loop in scan order, eliding
        the sort and short-circuiting a top-N (`query.order_by_join_scan`). _(Sub-follow-ons: `DESC`
        reverse outer scan; >2 relations; outer non-PK bound; `LEFT`/`RIGHT`/`FULL`; `DISTINCT`; and the
        index-order sub-follow-ons.)_
- [x] **`DISTINCT`** — NULL-safe dedup of projected rows, after ORDER BY before LIMIT; PG
      restriction on ORDER BY keys (`42P10`). → [grammar.md §11](spec/design/grammar.md)
- [x] **FROM-less `SELECT`** — `SELECT 1` over one virtual zero-column row.
      → [grammar.md §34](spec/design/grammar.md)
- [x] **Predicate forms — `IN (list)`, `BETWEEN`, `LIKE`, `CASE`** — IN/BETWEEN desugar; LIKE is a
      code-point matcher (`%`/`_`, `\` escape, `22025`); CASE is the engine's first lazy expression.
      → grammar.md §20–§23
  - [x] **`ILIKE`** — case-insensitive `LIKE` (landed with collation Slice 3e).
  - [x] **Regular expressions** — `~` `~*` `!~` `!~*` operators + `regexp_replace` / `regexp_match`,
        jed's own RE2-able flavor (a hand-written linear-time **Pike VM**, ReDoS-immune).
        → [regex.md](spec/design/regex.md)
  - [ ] _follow-on:_ LIKE `ESCAPE 'c'`; `SIMILAR TO` (deliberately excluded — the SQL-standard
        surface); set-returning `regexp_matches` / `regexp_split_to_table`; the Oracle-compat
        `regexp_count`/`instr`/`substr`/`like`; Unicode-property char classes (`\p{…}`),
        backreferences + lookaround (permanently out — they would break the linear-time guarantee).
- [x] **Aggregates `COUNT`/`SUM`/`MIN`/`MAX`/`AVG` + `GROUP BY` + `HAVING`** — first function-call
      syntax, whole-table + grouped aggregation, PG widening (SUM int→i64/decimal, AVG→decimal),
      grouping-error `42803`. → [aggregates.md](spec/design/aggregates.md)
  - [x] **`COUNT(DISTINCT x)` / aggregate `DISTINCT`** — fold only the distinct non-NULL argument
        values, deduplicated value-canonically; composes with GROUP BY (`query.aggregate_distinct`).
        → aggregates.md §5
  - [x] **`FILTER (WHERE cond)`** — `agg(args) FILTER (WHERE cond)` folds only the rows where `cond`
        is TRUE; composes with GROUP BY/HAVING/DISTINCT (`query.aggregate_filter`). → aggregates.md §11
  - [x] **`GROUPING SETS` / `ROLLUP` / `CUBE` + `GROUPING()`** — one GROUP BY names several grouping
        sets at once; an absent column projects NULL; `GROUPING()` → integer bitmask; total sets
        capped at 4096 (`54001`) (`query.grouping_sets`). → aggregates.md §12
  - [x] **Ordered-set aggregates** — `mode()` / `percentile_cont(f)` / `percentile_disc(f)`
        `WITHIN GROUP (ORDER BY col)`; `percentile_cont` is the interpolated **f64** percentile
        (PG-bit-identical), fraction a per-group constant f64 (`query.ordered_set_aggregate`).
        → aggregates.md §13
  - [x] **`SELECT DISTINCT` in an aggregate query** — dedups the projected grouped output rows, then
        LIMIT/OFFSET; the DISTINCT ORDER BY restriction applies (`query.aggregate_select_distinct`).
        → aggregates.md §14
  - [x] **`GROUP BY` by ordinal / output alias / general expression** — a grouping key may be a
        select-list ordinal (`42P10`), an output alias, or a general expression; composes with
        ROLLUP/CUBE/GROUPING SETS (`query.group_by_expr`). → aggregates.md §15
  - [x] **Functional-dependency grouping** — `GROUP BY` a base table's full PK lets its other columns
        appear ungrouped; whole composite PK, single grouping set (`query.group_by_functional_dependency`).
        → aggregates.md §16
  - [x] **`FILTER` on a window aggregate** — `agg(x) FILTER (WHERE cond) OVER (...)` folds only the
        frame rows where `cond` is TRUE; a non-aggregate window function with FILTER stays `0A000`
        (`query.window_aggregate_filter`). → aggregates.md §20
  - [x] **`GROUPING SETS` combined with window functions** — the window stage runs over the unioned
        grouping-set rows; `GROUPING()` and window functions coexist (`query.grouping_sets_window`).
        → aggregates.md §21
  - [x] **Non-constant ordered-set fraction** — `percentile_*(expr)` over grouping columns, evaluated
        per group (a non-grouped column is `42803`) (`query.ordered_set_nonconstant_fraction`).
        → aggregates.md §17
  - [x] **Interval input to `percentile_cont`** — interpolated in the interval domain (PG-byte-identical),
        result `interval` (`query.ordered_set_interval`). → aggregates.md §13
  - [x] **Array-valued `percentile_*` fraction** — computes one percentile per element, result an array
        (`query.ordered_set_array_fraction`). → aggregates.md §18
  - [x] **Collated `WITHIN GROUP` key** — `mode`/`percentile_disc` honor an explicit/column `COLLATE`
        in the sort; default byte (`C`) order (`query.ordered_set_collation`). → aggregates.md §13
  - [x] **Hypothetical-set aggregates** — `rank`/`dense_rank`/`percent_rank`/`cume_dist`
        `WITHIN GROUP (ORDER BY keys)`: the rank the hypothetical direct-arg row would have
        (`query.hypothetical_set_aggregate`). → aggregates.md §19
- [x] **Window functions (`OVER`)** — ✅ **COMPLETE (S0–S10, all three cores) + the sliding/sharing
      optimization.** Per-row values folded over a related row set in a dedicated window stage:
      ranking/offset/aggregate window functions, ROWS/RANGE/GROUPS frames + value offsets + EXCLUDE,
      the `WINDOW` named clause + base-window extension, general-expression `PARTITION BY`/`ORDER BY`
      keys, a shared partition/sort pass + frame sliding. Divergences D1/D2/D3 resolved; correlated
      window keys `0A000`. Deferred: prefix-compatible sort sharing, invertible moving slide, RANGE
      offsets over a ts/date key (D4), `FILTER`/`WITHIN GROUP`, `IGNORE NULLS`. → [window.md](spec/design/window.md)
- [x] **Scalar functions `abs` / `round`** — first named per-row functions (`kind = "function"`).
      → [functions.md §9](spec/design/functions.md)
  - [ ] _follow-on:_ `ceil`/`floor`/`mod`/`sign`, text `length`/`lower`/`upper`, a general implicit
        argument-coercion pass.
- [x] **Scalar string / text functions** — PG's string functions (manual §9.4) as `kind="function"`
      built-ins with code-point semantics (`length`/`substr`/`lpad`/`btrim`/`replace`/`translate`/
      `repeat`/`strpos`/`split_part`/`encode`/`decode`/`quote_*`/…).
      → [string-functions.md](spec/design/string-functions.md)
  - [ ] _follow-on:_ full-Unicode `initcap` word classification + non-ASCII titlecasing; keyword-aware
        `quote_ident`; a `text::bytea` cast + `length`/`octet_length`/`bit_length` `bytea` overloads;
        per-character cost metering for `lpad`/`rpad`/`repeat` (the §13 cost-ceiling path; the 54000
        hard cap is the current backstop).
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
> `timestamp`/`timestamptz`, `interval`, `bytea`, `uuid`, `f32`/`f64`, **`json`/`jsonb`**, and the
> `array` + composite containers are all done; only the deferred container follow-ons (keys + casts)
> and the JSON `0A000` follow-ons remain.

- [x] **Storable `boolean` column type** — on-disk type code 5, `bool-byte` codec, comparison +
      ORDER BY (false < true, NULLs last), boolean in a PK/index, and `boolean⇄i32` casts.
      → [types.md §9](spec/design/types.md), [casts.toml](spec/types/casts.toml)
- [x] **`text` + ONE collation (`C`)** — UTF-8 byte/code-point order, on-disk type code 4, the first
      operator overload, text in a PK/index/UNIQUE via the `text-terminated-escape` key encoding.
      → [types.md §11](spec/design/types.md), [encoding.md §2.4](spec/design/encoding.md)
  - [ ] _follow-on:_ `varchar(n)` length limits (`22001`); string functions
        (`||`, `length`, `lower`/`upper`, `substring`). _(Runtime non-literal text→T casts landed
        for the numeric + boolean scalars — see the typed-string-literal follow-on below.)_
  - [x] _follow-on:_ **linguistic collation (`COLLATE` / per-column / per-db default / UCA)** —
        slice 1 (a–e) landed: jed-owned UCA executor + compiler, `COLLATE`, per-column + per-db
        default, collated keys; deterministic collations only. → [collation.md](spec/design/collation.md)
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
      (settles the §8 rounding hotspot), PG result scales, finite-only (documented PG divergence),
      decimal in a PK/ordered index/UNIQUE via the scale-independent `decimal-order-preserving`
      encoding. → [decimal.md](spec/design/decimal.md), [encoding.md §2.5](spec/design/encoding.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods; `round(x,n)` and other decimal functions.
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, no tz database, `±infinity`
      first-class, timestamp PK supported. → [timestamp.md](spec/design/timestamp.md)
  - [x] **time-zone database + `AT TIME ZONE` (host-loaded `JTZ` bundle)** — Slice 1 (copies
        collation's host-load model): a host loads IANA tzdata as a `JTZ` bundle via `db.LoadTimeZoneData`
        (bare binary = `UTC` + fixed offsets); a per-core TZif reader + the `AT TIME ZONE` consumer. No
        `format_version` bump (`timestamptz` is UTC). → [timezones.md](spec/design/timezones.md), [tz/README.md](spec/tz/README.md)
  - [x] **the tz conversion surface (Slice 2)** — `date_trunc(unit, src[, zone])`,
        `EXTRACT(field FROM src)` → `numeric`, cross-family `timestamp`/`timestamptz`/`date` casts in a
        zone, and the observable session `TimeZone` slot (computation, not yet rendering).
        → [timezones.md §9](spec/design/timezones.md), [grammar.md §50](spec/design/grammar.md)
  - [ ] _further follow-on:_ `date_part` (float8 — needs `float`), `make_timestamptz`, `to_char`/
        `to_timestamp`, `age`, `EXTRACT(julian …)`; separate `time` type; **text⇄datetime casts** + the
        **session-zone rendering** of `timestamptz`; `timestamp(p)` precision typmods (timezones.md §9).
- [x] **`date`** — a calendar date (i32 days since 1970-01-01), strict ISO `YYYY-MM-DD` literals with
      BC era + `±infinity`, a date PK (on-disk type code 16, no format bump); a **strict island** — no
      compare/cast to timestamp this slice (a documented PG divergence). → [date.md](spec/design/date.md)
  - [x] _follow-on:_ **date arithmetic** — `date ± int` → date, `date − date` → i32 (days), `date ± interval`
        → timestamp (date widens to midnight); ±infinity-aware; 22008 on i32/timestamp-range overflow +
        infinite `date − date`; `date × other` = 42804 (PG 42883, override-ledgered); `date + bigint` accepted
        (one integer family, PG rejects). `date + time` needs a `time` type (deferred). → [date.md §6](spec/design/date.md)
  - [ ] _follow-on:_ **casts** (runtime text→date; `date ↔ timestamp`/`timestamptz` landed via tz §9.3);
        **clock-relative literals** (`today`/`tomorrow`/`yesterday`/`now`/`epoch`, entropy/clock seam);
        remaining **date functions** (`make_date`, `date_part`, `current_date`; `EXTRACT`/`date_trunc`
        landed). → [date.md §6](spec/design/date.md)
- [x] **Typed string literals + string-literal casts (`type 'string'`)** — one generalized production
      = `CAST('string' AS type)`; literal-only coercion preserves strictness. → [grammar.md §36](spec/design/grammar.md)
  - [x] _follow-on:_ **runtime text→T cast on a non-literal text expression** (shared with the text
        follow-on) — `CAST(text_col AS int)` / `s :: numeric(10,2)` / `s :: boolean` for the numeric +
        boolean scalars (`i16`/`i32`/`i64`, `decimal`, `f32`/`f64`, `boolean`), running the SAME
        per-type coercion the literal form folds at resolve but per-row in `evalCast` (the existing
        `operator_eval` charge meters it; `22P02`/`22003` per row; jed's own grammar, so hex/underscore/
        `NaN` trap `22P02` — per-core tested). New cap `cast.runtime_text`; oracle-clean
        [text_to_scalar.test](spec/conformance/suites/cast/text_to_scalar.test). text→`uuid` already
        landed; text→`date`/`timestamp`/`timestamptz`/`interval`/`bytea` stay deferred to each type's
        own follow-on. → [grammar.md §36](spec/design/grammar.md), [casts.toml](spec/types/casts.toml)
- [x] **`::` cast operator** (`expr :: type`) — desugars to the `Cast` node; binds tighter than
      unary minus; a bind-param operand takes the cast target as its type. → [grammar.md §37](spec/design/grammar.md)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, the
      engine's first timestamp arithmetic; on-disk type code 11; interval PK/index/UNIQUE/FK/GIN via the
      16-byte `interval-span-i128` span key. → [interval.md](spec/design/interval.md), [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ CAST to/from interval; ISO-8601 `P…` + SQL-standard
        input; field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `justify_*`/`EXTRACT`/`age`.
- [x] **`bytea`** — variable-width bytes, unsigned byte order, `\x`-hex literals (`22P02` on bad hex),
      on-disk type code 7; bytea PK/index/UNIQUE via the `bytea-terminated-escape` key encoding.
      → [types.md §13](spec/design/types.md), [encoding.md §2.6](spec/design/encoding.md)
  - [ ] _follow-on:_ traditional escape input (`\nnn`); bytea⇄other casts; binary functions
        (`length`, `||`, `substring`, `encode`/`decode`, `get_byte`).
- [x] **`uuid`** — fixed 16 bytes, PG-flexible input, canonical lowercase output, on-disk type code 8;
      the **first non-integer `PRIMARY KEY`** (`uuid-raw16` key encoding). → [types.md §14](spec/design/types.md)
  - [x] _follow-on:_ uuid⇄other casts (`text ⇄ uuid`, `bytea ⇄ uuid`) — four explicit pairs.
        `text → uuid` (PG-flexible `uuid_in`, 22P02) and `uuid → text` (canonical lowercase)
        oracle-checked; `uuid ⇄ bytea` (the 16 raw bytes, 22P02 on length≠16) is a jed cast PG
        lacks, per-core tests. → [types.md §14](spec/design/types.md), [casts.toml](spec/types/casts.toml)
- [x] **uuid extractor functions** — `uuid_extract_version` / `uuid_extract_timestamp` (immutable);
      landed the catalog `volatility` field. → [functions.md §12](spec/design/functions.md)
- [x] **uuid generator functions** — `uuidv4()` / `uuidv7([shift])`; landed the host-injected
      entropy+clock seam (splitmix64 PRNG). → [entropy.md](spec/design/entropy.md)
- [x] **Current-time functions** — `now()` (STABLE) / `current_timestamp` (sugar) /
      `clock_timestamp()` (VOLATILE) on the clock seam. → [functions.md §12](spec/design/functions.md)
- [x] **`f32` + `f64` (IEEE 754)** — two-width promotion tower; the first types **narrowly** exempted
      from cross-core byte-identity (via the `R` tag's tolerant compare); established the determinism
      framework + exception ledger; on-disk type code 12. → [float.md](spec/design/float.md),
      [determinism.md](spec/design/determinism.md)
  - [ ] _follow-on:_ float in a PRIMARY KEY/index (`0A000`); key rule authored, unexercised.
  - [x] **math functions** — the [float.md §8](spec/design/float.md) follow-ons, all three cores,
        oracle-clean: `cbrt`/`pi()`/`radians`/`degrees`, the inverse + hyperbolic trig, `power`; and
        the EXACT numerics `sign`/`mod()`/`div()`/`gcd`/`lcm`/`factorial`/`width_bucket` (new `2201G`)/
        `scale`/`min_scale`/`trim_scale`.
    - [ ] _deferred:_ the **exact-numeric** transcendentals `power(numeric,numeric)`, `log(numeric)`,
          `log(b, x)` — must be byte-identical cross-core (decimal is in-contract, no ULP exemption),
          so they need a PG-faithful arbitrary-precision `ln`/`exp`/`power` port (numeric.c). Also the
          `width_bucket(value, thresholds[])` array-threshold variant.
- [x] **`json` / `jsonb` + SQL/JSON** — ✅ the committed XL headline feature (§1, §4): **all
      non-deferred slices landed** across all three cores, oracle-clean. Designed spec-first across four
      docs ([json.md](spec/design/json.md), [jsonpath.md](spec/design/jsonpath.md),
      [json-sql-functions.md](spec/design/json-sql-functions.md), [json-table.md](spec/design/json-table.md));
      stable type codes 18/19/20, one `format_version` bump (v18→v19) at J1.
  - [ ] _follow-ons (deferred `0A000`, hoisted from the done slices):_ the string-**dictionary builder**
        (opens the [json.md §3](spec/design/json.md) door); `jsonb`-as-PK/index (exercise
        [encoding.md §2.13](spec/design/encoding.md)); GIN **`jsonb_ops`** opclass for `@>`/`?` (the
        [gin.md](spec/design/gin.md) seam already seats it); `JSON_TABLE` explicit `PLAN` (T2);
        `ON ERROR/EMPTY DEFAULT <expr>` (S3, the guarded sub-evaluation); the remaining **jsonpath**
        surface (`like_regex` → Pike VM, item methods `.type()`/`.size()`/`.double()`/…, arithmetic,
        `vars`/`silent` args, the `_tz` query-function variants — P2/P3); the **verbatim-`json`** SRF /
        accessor variants (`json_array_elements[_text]`, `json_each[_text]`, the `->`/`#>` json overloads);
        `jsonb_set_lax`; `row_to_json` (composite→json, riding the `to_jsonb`-composite un-deferral);
        in-aggregate `ORDER BY` for `json[b]_agg`.
- [ ] **`array` type** — the **second container axis** (sibling to composite, sharing ~80% of its
      foundation): a **structural** `Type::Array(Box<Type>)` over any element type, with array
      *shape* a property of the value (PG-faithful), the compact null-bitmap value codec,
      btree-NULL element comparison (*not* composite 3VL), and `array_in`/`array_out`. **Landed:**
      S0–S5 (`format_version` 10; subscripting `a[i]` in S3; multidim values + custom lower bounds +
      slices `a[m:n]` in S5); the **AF1–AF7 function/operator surface** (`anyarray`/`anyelement`
      polymorphism, introspectors, builders, `||`/`@>`/`<@`/`&&`, `unnest`, `ANY`/`ALL`/`SOME` + the
      subquery quantifier form, `VARIADIC`); and **composite-element arrays** (AC1, the composite
      array-field CMP-ARR-FIELD, `unnest(composite[])` AF7); and the **three runtime array casts**
      (capability `cast.array`, array.md §7 — runtime text→`T[]`, `array::text` (explicit-only), and
      element-wise array→array; no format bump). → [array.md](spec/design/array.md), [array-functions.md](spec/design/array-functions.md)
  - [ ] _remaining follow-on (its own slice + obligations):_ **arrays-in-keys** (`0A000`, encoding
        authored array.md §8) — the order-preserving array key encoding, so an array `PRIMARY KEY` /
        secondary index / `UNIQUE` / FK target (the text/decimal/bytea/composite-key precedent).
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — ✅ **COMPLETE (S0–S6).** The
      **second container axis** turning the *closed* type enum into an *open* one: `Type { Scalar |
      Composite(catalog-ref) }` threaded through all three cores; `CREATE`/`DROP TYPE`, nested +
      recursive types, a storable composite column + recursive value codec (`format_version` 9), `ROW(…)`
      construction, field access, element-wise compare/ORDER BY/DISTINCT/GROUP BY. Named composites only.
      → [composite.md](spec/design/composite.md)
  - [ ] _still narrowed (relaxable later):_ `INSERT … SELECT` / `UPDATE` of a composite column;
        composite `PRIMARY KEY`/index/`UNIQUE` (`0A000` — key encoding authored, unexercised);
        `DEFAULT` on a composite column; runtime non-literal text→composite + `composite::text` +
        anonymous `ROW(…)::type` casts; the nested `ROW(ROW(…),…)`-into-column constructor.

---

## Phase 4 — Relational depth + constraints

> The meaty planner/executor work and the rest of the integrity story.

- [x] **`JOIN` — multi-table FROM + `INNER`/`CROSS` + outer (`LEFT`/`RIGHT`/`FULL`)** — left-deep
      nested-loop executor, table aliases, qualified column refs, a flat-index scope resolver; the outer
      NULL-extension branch + WHERE-downgrades-to-inner. → [grammar.md §15](spec/design/grammar.md)
  - [ ] _follow-on:_ `USING` / `NATURAL` / comma-`FROM` / `t.*`.
- [x] **Subqueries** — uncorrelated scalar, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS`, **correlated**,
      subqueries in `UPDATE`/`DELETE`, `$N` inside a subquery, **derived tables**, a `VALUES` body,
      **`LATERAL`**, and `x op ANY/ALL(SELECT …)`.
      → [grammar.md §26/§42/§44](spec/design/grammar.md), [array-functions.md §11.6](spec/design/array-functions.md)
  - [ ] _follow-on:_ a correlated `GROUP BY` / `ORDER BY` key (`0A000`, degenerate).
  - [ ] _follow-on:_ a **parenthesized-join FROM** (`FROM (a JOIN b ON …)`); a trailing **`ORDER
        BY`/`LIMIT` on a VALUES body**; **comma-`FROM`** (`FROM t, LATERAL (…)`) — until it lands,
        LATERAL is reached only through explicit `JOIN` syntax.
  - [ ] **Subqueries — remaining seams:** subqueries in an **`INSERT ... VALUES`** slot (blocked on
        VALUES holding a general expression); **row-valued** subqueries. _(size: S)_
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — a query-expression
      precedence tree (INTERSECT binds tighter), full-PG per-column type unification, NULL-safe multiset
      semantics, trailing ORDER BY by output-column name or ordinal. → [grammar.md §25](spec/design/grammar.md)
  - [ ] _follow-on:_ parenthesized operands `(SELECT …) UNION …`; ORDER BY/LIMIT inside an operand;
        ORDER BY ordinals; a set op in an `INSERT … SELECT` source.
- [x] **Common table expressions (`WITH`)** — named derived tables in FROM (PG hybrid
      inline/materialize rule), plus **`WITH RECURSIVE`** (iterate-to-fixpoint), **data-modifying
      (writable) CTEs** (one pre-statement snapshot, all-or-nothing), and **nested `WITH`**.
      → [cte.md](spec/design/cte.md), [recursive-cte.md](spec/design/recursive-cte.md), [writable-cte.md](spec/design/writable-cte.md)
  - [ ] _follow-on:_ a nested `WITH` **inheriting enclosing CTEs** (the residual visibility divergence
        above); recursive-CTE deferrals (`SEARCH`/`CYCLE`, a set-op / `FROM`-subquery recursive term,
        mutual recursion).
- [x] **Set-returning functions** — `generate_series(start, stop [, step])` in FROM position, a
      synthetic one-column relation, a `generated_row` cost unit; integer variants. → [functions.md §10](spec/design/functions.md)
  - [ ] _follow-on:_ the column-alias-list `AS g(c)`. (`LATERAL` ✅ landed — an SRF is implicitly
        lateral, [grammar.md §44](spec/design/grammar.md); `unnest(array)` ✅ landed — AF3.)
- [x] **`NOT NULL`** — explicit column constraint; storing NULL → `23502`.
      → [constraints.md §1](spec/design/constraints.md)
- [x] **`DEFAULT` (literal)** — evaluated + coerced once at CREATE TABLE; landed with the INSERT
      column list + the `DEFAULT` value keyword. → [constraints.md §2](spec/design/constraints.md)
- [x] **`DEFAULT` (expression)** — non-constant `DEFAULT <expr>` (e.g. `uuidv7()`, `1 + 1`) stored as
      expression text + evaluated per row at INSERT through the entropy/clock seam; `format_version` 8.
      → [constraints.md §2](spec/design/constraints.md)
  - [ ] _follow-on:_ `UPDATE ... SET x = DEFAULT` and `INSERT ... DEFAULT VALUES`.
- [x] **Composite `PRIMARY KEY`** — table-level `PRIMARY KEY (a, b, …)`; key bytes = members'
      concatenated encodings. → [constraints.md §3](spec/design/constraints.md)
  - [ ] _follow-on:_ composite point-lookup / prefix pushdown (a composite-PK table full-scans today
        — an optimization slice with its NoREC obligation).
- [x] **`CHECK` constraints** — column- + table-level `[CONSTRAINT name] CHECK (expr)`, enforced per
      candidate row inside the two-phase pass (`23514`), PG auto-naming; `format_version` 4.
      → [constraints.md §4](spec/design/constraints.md)
- [x] **`UNIQUE` constraints + unique indexes** — column-/table-level `UNIQUE` and `CREATE UNIQUE
      INDEX`; a UNIQUE constraint **is** its backing unique index; NULLS-distinct; `format_version` 6.
      → [constraints.md §5](spec/design/constraints.md), [indexes.md §8](spec/design/indexes.md)
- [x] **`FOREIGN KEY` constraints** — column- + table-level `REFERENCES`; composite + self-reference;
      referenced columns must be the parent PK or a UNIQUE set (`42830`), same-type pairing (`42804`,
      stricter than PG); MATCH SIMPLE; enforced at four write sites (`23503`) in the two-phase pass;
      `format_version` **11**. → [constraints.md §6](spec/design/constraints.md), [grammar.md §43](spec/design/grammar.md)
  - [ ] _follow-on:_ the referential **actions** `ON DELETE/UPDATE CASCADE | SET NULL | SET DEFAULT`
        (parse but `0A000` today — they write the child during a parent mutation); `MATCH FULL`;
        a **backing index** on the child FK columns (the parent-side check full-scans children today);
        FK type pairing relaxed to PG's comparable-types; `ALTER TABLE … ADD/DROP CONSTRAINT`.
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — non-unique on-disk B-trees of
      empty-payload records, maintained in the two-phase pass; the planner index-bounds a SELECT base
      scan on a first-column equality; `format_version` 5 catalog reshape. → [indexes.md](spec/design/indexes.md)
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
- [x] **GiST index access method → `EXCLUDE` constraints** ✅ **DONE** (GX0–GX3, all three cores +
      Ruby golden, byte-identical) — a **third index *kind*** (`index_kind = 2`) beside the ordered
      B-tree and GIN, whose payoff is **PostgreSQL exclusion constraints** (`EXCLUDE USING gist (col
      WITH op)`, `23P01`). jed's GiST is an operation-deterministic R-tree (a structural divergence
      from PG — same rows match, jed's own tree bytes); the `range_ops` + fixed-width scalar-`=`
      opclasses; multi-column GiST; `format_version` 21. → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ the `EXCLUDE … WHERE (predicate)`
        partial form; `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery yet —
        its own axis); `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique
        index (a `UNIQUE` alias); `ALTER TABLE … ADD CONSTRAINT … EXCLUDE`; **SP-GiST** (`index_kind = 3`)
        and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH
        operands; multi-column GiST beyond the exclusion shape.
  - [ ] _follow-on — future GiST opclasses (each its own slice, gated on its type/operator surface
        landing first; all anticipated by GX0's general seam):_ **`multirange_ops`** once a multirange
        type lands ([ranges.md §10](spec/design/ranges.md) — a recognized future want); an
        **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type — a new type axis,
        or riding the [json.md §3](spec/design/json.md) dictionary door — which would bring a **GIN**
        opclass too ([gin.md §10](spec/design/gin.md)); a **`pg_trgm`-style trigram `text` opclass**
        accelerating similarity (`%`) / `LIKE` / `ILIKE` (jed's regex is its own flavor,
        [regex.md](spec/design/regex.md), so accelerating it needs care — likely LIKE/ILIKE first); and an
        **`intarray`-style signature GiST opclass** over array columns (an alternative to the shipped GIN
        `array_ops`), alongside the extra intarray query operators. Each is "build it when its type /
        operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <select_items>` projecting affected
      rows, evaluated after validation before any write; the PG-18 `old.`/`new.` row-version qualifiers
      landed as a follow-on. → [grammar.md §32](spec/design/grammar.md)
  - [ ] _follow-on:_ the `WITH (OLD AS o, NEW AS n)` aliasing form; `old.*`/`new.*`.
- [x] **Sequences** (`CREATE SEQUENCE` / `nextval` / `currval`) — ✅ **landed (S0–S6):** a persisted
      monotonic i64 generator (`entry_kind = 2`, `format_version` 12), `nextval`/`currval`/`setval`/
      `lastval`, `serial`/IDENTITY owned-sequence columns (`format_version` 14/15), `ALTER SEQUENCE`.
      **The defining decision — `nextval` is TRANSACTIONAL** (a deliberate PG divergence, determinism.md
      §5). → [sequences.md](spec/design/sequences.md)
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET …
      [WHERE …] }`: a candidate row that would violate a UNIQUE/PK constraint takes the conflict action
      instead of `23505`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter;
      two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT` (with the `UPDATE` `SET = DEFAULT` follow-on);
        `INSERT INTO t AS alias` (the existing row is referenced by the table name today); the
        partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing
        the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed,
        but the conflict-path re-key (the existing row moves) is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [x] **Relax the UPDATE narrowings** — assigning a `PRIMARY KEY` column now **re-keys** the row (it
      moves, secondary-index entries follow); end-state validation traps `23505`/`23503`, an
      end-state-valid swap/cascade succeeds where PG fails the per-row transient. No `format_version`
      bump; the DO UPDATE conflict-path equivalent remains a deferred `0A000` follow-on. (§11 step 6.)
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
      structurally-shared clone), `TableStore` wrap, autocommit through the single `persist` chokepoint.
- [x] **P5.2 — explicit transactions** — SQL `BEGIN`/`COMMIT`/`ROLLBACK` (+ `READ ONLY|WRITE`) and the
      `Transaction` API; a current-transaction state machine, class-25 errors.
      → [grammar.md §27](spec/design/grammar.md), [api.md §6](spec/design/api.md)
- [x] **P5.3 — reader/writer concurrency + the watermark** — immutable `Snapshot` + a `SharedDb` handle
      realizing concurrent readers + a single writer with the live-reader registry (`oldest_live_txid`,
      the Phase-6 free-list gate). → [transactions.md §8/§10](spec/design/transactions.md), [api.md §2.5](spec/design/api.md)
- [x] **P5.4 — cross-core concurrency conformance** — the `# format: concurrency` schedule (Layer 1),
      the write-gate `blocks` annotation (Layer 2), and the `stress/*.stress.toml` parallelism-stress
      format (Layer 3, `rake stress`, outside `rake ci`). → [concurrency-testing.md](spec/design/concurrency-testing.md)

---

## Phase 6 — Storage maturation (§9)

> Can lag the feature work until write volume makes whole-image rewrites costly.
>
> **TB-scale non-foreclosure (CLAUDE.md §9):** these items are also the path to a
> **larger-than-RAM file that does not fall over**. RAM-sized is the dominant case but not a
> hard limit — present work must not foreclose >>RAM operation (no full-residency assumption
> above the storage seam; no operator that requires its whole input/output in RAM).

- [x] **P6.1 — incremental COW commit = page-backed B-tree** — whole-image serialize replaced by
      dirty-page-only writes + meta-slot root swap; `format_version` 2 byte contract, 15 goldens
      byte-exact `rust==go==ts==ruby`. → [storage.md §4/§6](spec/design/storage.md)
- [x] **P6.2 — free-list / page reclamation** _(reconstruct-on-open form)_ — the free-list is rebuilt
      on open and the commit allocator reuses it lowest-index first; torn-write-safe, gated on the
      oldest-live watermark; byte format unchanged. → [transactions.md §8](spec/design/transactions.md)
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
      unit (structural node count — a future buffer pool stays invisible), re-baselined atomically
      across all three cores. → [cost.md §3](spec/design/cost.md), [schedule.toml](spec/cost/schedule.toml)
- [x] **P6.4 — buffer pool / demand paging** — the resident set is now a bounded cache of **leaf**
      pages with CLOCK eviction (interior skeleton stays resident, so `page_read` stays structural); a
      handle-level `cache_pages` budget (default 1024). → [pager.md](spec/design/pager.md), [api.md §2.1](spec/design/api.md)
- [ ] **Streaming + spill-to-disk operators** — bound blocking operators (`ORDER BY`, hash
      `JOIN`, `GROUP BY`/aggregate, `DISTINCT`) by a memory budget and **spill to disk** when
      exceeded (external merge sort, grace hash join), so a query over larger-than-RAM data
      never materializes its whole input/output in memory. Designed in
      [spill.md](spec/design/spill.md). _(size: XL; deps: paged storage; §9/§13)_
  - [x] **External merge sort for `ORDER BY`** — a `Sorter` bounded by `work_mem` (default 256 MiB)
        spills sorted runs + k-way merges, reproducing the in-memory stable sort byte-for-byte;
        result- and cost-invariant.
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
            `fsync`→`fdatasync`, so steady-state commits overwrite already-allocated space: ~9.0ms →
            ~2.5–3.1ms p50 (~2.7×), identical cross-core checksums. → [pager.md §7](spec/design/pager.md)
- [x] **Large values — overflow pages + compression (TOAST-equivalent)** — large `text`/`bytea`/
      `decimal`/`json` pushed out-of-line onto overflow-page chains (Slice A, `format_version` 3),
      optionally LZ4-compressed first via a deterministic hand-rolled block codec (Slice B, no
      third-party dependency — a library fails §8 byte-identity).
      → [large-values.md](spec/design/large-values.md), [lz4.md](spec/fileformat/lz4.md)
  - [ ] _follow-on:_ chain sharing on rewrite (let a rewritten record keep an unchanged value's
        existing chain — a byte-layout change, lands in all cores + incremental tests together).
- [x] **Crash-recovery hardening** — a pager fault-injection seam + a cross-core recovery matrix
      proving a crash *anywhere* recovers to a valid snapshot, never corrupt; WAL stays deferred (COW +
      root-swap gives atomicity without one). → [storage.md §7](spec/design/storage.md)

---

## Phase 7 — Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters have
> landed; the browser/OPFS host remains. Parallelizable with most feature work.

- [x] **Formal public API** — `create`/`open`, crash-safe explicit `commit` / `close`, `prepare`,
      execute, a `Rows` cursor, structured errors; same shape across all three cores.
      → [api.md](spec/design/api.md)
- [x] **Parameterized queries (`$1`)** end-to-end — lexed/parsed, context-typed at resolve (`42P18`
      if indeterminate), bound two-phase before any scan; tested per-core (corpus stays literal-only).
- [ ] **Storage hosts** — formal interface authored in [hosts.md](spec/design/hosts.md): the
      five-method `BlockStore` byte device, the host catalog, the decoration layering (encryption
      codec above the seam, replication tee below). Node `fs` host built; Rust/Go inline
      `std::fs`/`os` in the per-core `Pager`. **Landed:** the `FileBlockStore` extraction and the
      **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity
      vs goldens, gated Playwright e2e). Deferred: OPFS disk-spill, the e2e in CI. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [x] **Cost ceiling (`max_cost`) + deterministic abort** — a handle `max_cost` aborts a statement
      with `54P01` the instant accrued cost reaches it; plus a fixed `MAX_EXPR_DEPTH = 256` parser
      nesting bound (`54001`) closing the native-stack-overflow gap. → [cost.md §6/§7](spec/design/cost.md)
- [x] **The `jed` CLI** — a full-screen TUI client (Rust + ratatui/crossterm/tui-textarea) + a plain
      script mode (`-c`/`-f`/stdin; aligned/csv/json). A host program, not a core. → [cli.md](spec/design/cli.md)
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
  - [x] **S6 — session time zone slot** — the built-in `time_zone` var (default **`UTC`**; named loaded
        zones else `22023`), injected not OS-read; the `# timezone:` directive; the consumers
        (`date_trunc`/`EXTRACT`/cross-family casts) landed too (`session.timezone`). _(§6.2)_
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
- [ ] **Ruby** gem — **wrap** the Rust core, shipped as a gem. The clear-cut reach call, blessed
      in [cores.md](spec/design/cores.md) §6 item 4 ("ship Ruby … as a wrapper, gem → Rust") and
      §3 (Ruby's GC'd, dynamic runtime surfaces **no** new divergence axis Go doesn't already
      cover — a native core is unjustified; "Ruby is not the interesting case"). Wraps the **safe
      Rust** core (§2/§13), so memory-safety, the pure built-in surface, and the cost meter carry
      through unchanged — a wrap can't weaken them because it *is* the engine. Conforms **by
      construction** (inherits Rust's corpus pass + byte-exact round-trip), a distribution
      artifact, **not** an independent conformance voice (echoes Rust, zero new divergence — §2;
      cores.md §1). **Not** the existing **Ruby file-format reference**
      ([spec/fileformat/verify.rb](spec/fileformat/verify.rb), the independent fourth
      encoder/decoder behind the golden tests, `rust == go == ts == ruby`) — that stays a
      reference reader/writer; this is a shippable gem over the whole engine.
  - [x] **Slice 1 — the gem + the FFI seam.** A standalone `cdylib` crate (`impl/ruby/ext`) exposing
        an 8-function C ABI (the **only `unsafe` in the product path**, `catch_unwind`-guarded); a
        pure-Ruby gem (`impl/ruby/lib`) loads it through stdlib **`fiddle`** (no third-party dep) as
        idiomatic `Jed::Database`/`Result`/`Row`/`Error`. → [ruby.md](spec/design/ruby.md)
  - [x] **Slice 2 — `$N` bind parameters.** `execute(sql, *params)`/`query(sql, *params)` marshal
        Ruby scalars into a length-delimited param buffer (ABI v2); the engine context-types + coerces
        each `$N` two-phase. → [ruby.md §3a](spec/design/ruby.md)
  - [x] **Slice 3 — richer typed values (AR-style, always-on).** `decimal`⇄`BigDecimal`,
        `date`⇄`Date`, `timestamp`/`timestamptz`⇄`Time` (UTC), read + bind (ABI v3), mirroring
        ActiveRecord (incl. `±infinity` → `±Float::INFINITY`); adds the **`bigdecimal`** gemspec dep
        (a Ruby bundled stdlib gem). → [ruby.md §3](spec/design/ruby.md)
  - [x] **Slice 4 — host-loaded bundles.** `Jed.load_unicode_data(bytes)` /
        `Jed.load_time_zone_data(bytes)` over the engine-global seams (ABI v4): loading a JUCD/JTZ
        byte bundle adds `COLLATE "unicode"`/ILIKE/case-folding and named zones. → [ruby.md §5a](spec/design/ruby.md)
  - [x] **Binding-overhead benchmark** (`bench/ruby`, `jed/ruby/wrap`) — runs the shared corpus
        through the gem; its `ns_per_op` delta vs `jed/rust/core` is the wrapper tax; reuses the
        splitmix64 PRNG + FNV-1a checksum, also reports allocations/op. → [benchmarks.md §7.1](spec/design/benchmarks.md)
  - [ ] _follow-on (each its own slice):_ **gem prepared-statement API** (isolates the pure FFI tax
        from the per-call parse the overhead bench currently includes); **`interval`/`uuid`/`bytea`
        typed coercion** (left as String — no single obvious native target); **distributable packaging**
        — a `gem install`-able native gem via **`rb-sys` + precompiled platform gems** (or
        `magnus` for richer Rust ergonomics), replacing the in-repo `rake ruby:build` step (a
        wrapper-module dep, the `bench/`/`web/` precedent §14, needs the §14 confirmation before
        adding); an optional **Ruby conformance runner**. In-process Ruby host functions pay the
        FFI upcall tax (the §2.1 hotness pivot), so they ride on the **vectorized/batched
        host-function API** below. _(size: L wrap; §2/§13)_
- [x] **Runtime function registry — the §5 dispatch foundation** — resolution for built-in named
      scalar functions + aggregates is now data-driven over the generated catalog tables (one
      `(name, arg_families)` lookup). → [extensibility.md §5](spec/design/extensibility.md)
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
