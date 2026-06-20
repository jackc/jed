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
- [x] **`DISTINCT`** — NULL-safe dedup of projected rows, after ORDER BY before LIMIT; PG
      restriction on ORDER BY keys (`42P10`). → [grammar.md §11](spec/design/grammar.md)
- [x] **FROM-less `SELECT`** — `SELECT 1` over one virtual zero-column row.
      → [grammar.md §34](spec/design/grammar.md)
- [x] **Predicate forms — `IN (list)`, `BETWEEN`, `LIKE`, `CASE`** — IN/BETWEEN desugar to
      `=`/`OR`/`AND`/`NOT`; LIKE is a code-point matcher (`%`/`_`, `\` escape, `22025`); CASE is
      the engine's first lazy expression. → grammar.md §20–§23
  - [ ] _follow-on:_ LIKE `ESCAPE 'c'`, `ILIKE`, `SIMILAR TO`.
- [x] **Aggregates `COUNT`/`SUM`/`MIN`/`MAX`/`AVG` + `GROUP BY` + `HAVING`** — first
      function-call syntax, whole-table + grouped aggregation, PG widening (SUM int→i64/decimal,
      AVG→decimal), grouping-error `42803`. → [aggregates.md](spec/design/aggregates.md)
  - [ ] _follow-on:_ `COUNT(DISTINCT x)`, `SELECT DISTINCT` in an aggregate query, GROUP BY by
        expression/ordinal/alias, functional-dependency grouping, `GROUPING SETS`/`FILTER`/ordered-set.
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
      ORDER BY (false < true, NULLs last). → [types.md](spec/design/types.md)
  - [x] **boolean in a key / `PRIMARY KEY`** — ✅ landed: the `bool-byte` key encoding is
        exercised (the second non-integer key after uuid; encoding.md §2.9), covering a boolean
        PRIMARY KEY, a composite-key member, and a secondary index — with point lookup, 23505 on a
        duplicate key, 23502 on a NULL key, and the `bool_pk_table.jed` golden + `integers.toml`
        boolean key vectors pinning the bytes cross-core.
  - [x] **boolean⇄i32 casts** — ✅ landed: `boolean → i32` (true→1 / false→0) and `i32 → boolean`
        (0→false, any nonzero→true), both **explicit**, **i32 only** (PG ties the boolean↔integer cast
        to `int4`; `NULL`→`NULL`, `bool→bool` identity). An integer literal operand of a boolean target
        adapts to i32 (`5::boolean`). A `bool⇄i16` / `bool⇄i64` pair is a forbidden `42804` (PG `42846`
        — a documented divergence, jed's standing forbidden-cast convention; per-core unit tests). New
        `cast.bool_int` capability + `suites/cast/bool_int.test` (oracle-clean); no format change. →
        [types.md §9](spec/design/types.md), [casts.toml](spec/types/casts.toml)
- [x] **`text` + ONE collation (`C`)** — UTF-8 byte/code-point order, on-disk type code 4, first
      operator overload; the UTF-8-vs-UTF-16 ordering trap handled in TS. → [types.md §11](spec/design/types.md)
  - [x] **text in a PRIMARY KEY/index/UNIQUE** — ✅ landed: the `text-terminated-escape` key
        encoding (encoding.md §2.4) is exercised (the first variable-width non-integer key), with
        byte fixtures (`spec/encoding/text.toml`) + the `text_pk_table.jed` golden; an oversized
        text key is `0A000`. → [encoding.md §2.4](spec/design/encoding.md)
  - [ ] _follow-on:_ `varchar(n)` length limits (`22001`); runtime non-literal text→T casts;
        string functions (`||`, `length`, `lower`/`upper`, `substring`).
  - [ ] _follow-on:_ **linguistic collation (`COLLATE` / per-column / per-db default / UCA)** —
        design ratified in [collation.md](spec/design/collation.md): jed-owned UCA executor +
        compiler, no tables vendored in the binary. A collation is a first-class **portable
        artifact** — produce (extract from host / compile / open) → save → import/export — **baked
        into the db file** by default (zero index drift, self-describing) with a small-footprint
        name+hash **reference mode** and an optional **provenance description**; deterministic
        collations first. Slice plan in collation.md §14.
- [x] **Exact `decimal`** — *the* headline type: hand-rolled sign+coefficient+scale, round-half-away
      (settles the §8 rounding hotspot), PG result scales, first parameterized + first cross-family
      promotion; finite-only (documented PG divergence). → [decimal.md](spec/design/decimal.md)
  - [x] _follow-on:_ decimal in a PRIMARY KEY / ordered index / UNIQUE key — the order-preserving,
        scale-independent `decimal-order-preserving` encoding ([encoding.md](spec/design/encoding.md)
        §2.5; `1.5` and `1.50` index as one). → still deferred: negative / `s>p` scale typmods;
        `round(x,n)` and other decimal functions.
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, no tz database, `±infinity`
      first-class, timestamp PK supported. → [timestamp.md](spec/design/timestamp.md)
  - [ ] _follow-on:_ `EXTRACT`/`date_trunc`/`age`; separate `time` type; named-zone
        `AT TIME ZONE`; timestamp⇄text/date casts; `timestamp(p)` precision typmods.
        (`date` ✅ landed below.)
- [x] **`date`** — a calendar date (year/month/day, no time/zone): i32 days since 1970-01-01,
      reusing timestamp's calendar core; strict ISO `YYYY-MM-DD` literals (string-adapt + `DATE '…'`
      keyword) with BC era + `±infinity`, a trailing time/offset validated then dropped (24:00:00
      does NOT roll into the day, unlike timestamp), comparison/ordering by the day count, a date
      PRIMARY KEY (key encoding = i32; on-disk type code 16, no `format_version` bump). A **strict
      island** — no compare/cast to timestamp this slice (a documented PG divergence). jed owns a
      wider range than PG (≈ ±5.88M years). → [date.md](spec/design/date.md)
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
      engine's first timestamp arithmetic; on-disk type code 11. → [interval.md](spec/design/interval.md)
  - [x] interval PK/index — the `interval-span-i128` 16-byte span key (PRIMARY KEY / ordered index /
        UNIQUE / FK target / GIN element); span-equal values share a key. → [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ CAST to/from interval; ISO-8601 `P…` + SQL-standard
        input; field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `justify_*`/`EXTRACT`/`age`.
- [x] **`bytea`** — variable-width bytes, unsigned byte order, `\x`-hex literals (`22P02` on bad
      hex), on-disk type code 7. → [types.md §13](spec/design/types.md)
  - [x] **bytea PK/index/UNIQUE** — ✅ landed: the `bytea-terminated-escape` key encoding
        (encoding.md §2.6, like text but over raw bytes — the embedded-0x00 escape is routinely
        hit), with byte fixtures (`spec/encoding/bytea.toml`) + the `bytea_pk_table.jed` golden.
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
      *shape* a property of the value (PG-faithful), the compact null-bitmap value codec (no
      per-element prefix for fixed-width elements), btree-NULL element comparison (*not* composite
      3VL), and `array_in`/`array_out`. **S0–S5 landed** (`format_version` 10; subscripting `a[i]`
      in S3; multidim values + custom lower bounds + slices `a[m:n]` in S5). Spec'd in
      [array.md](spec/design/array.md); decisions §10, errors §11, delivery §12. _(size: XL; §4/§8)_
  - [x] **S0** — `spec/design/array.md` + the CLAUDE.md §4 array-axis touch (structural; shape is a
        value property) + this slice breakdown + the §10 decisions + §11 error surface.
  - [x] **S1** — the open-`Type` `Array(Box<Type>)` arm threaded through parser/resolver/evaluator,
        behavior-preserving (composite already opened `Type`, so additive). _(size: M)_
  - [x] **S2** — declarable + storable array **column** (scalar elements) + `type_code = 15` + the
        value codec ([array.md](spec/design/array.md) §4) + `format_version` 10 + new goldens
        (`array_table.jed`, `rust == go == ts == ruby`); the `ARRAY[…]` constructor + `'{…}'`/`::`
        literal (`array_in`) + INSERT/SELECT round-trip + `array_out` rendering — all three cores +
        Ruby byte-identical. 1-D values only. _(size: L)_
  - [x] **S3** — subscripting `a[i]` (1-based; OOB/NULL → NULL; non-array base `42804`) — a postfix
        `[…]` on any base, all three cores + `types/subscript.test`. _(size: S)_
  - [x] **S4** — comparison / ordering / `IS NULL`: same-element-type comparable (`42804`
        otherwise), the **btree-NULL** element-wise `eq3`/`lt3`/`gt3` (§5 — *not* composite 3VL), the
        `ORDER BY` total-order arm, DISTINCT/GROUP BY array keys, whole-value-only `IS NULL`;
        oracle-pinned via `rake corpus:check`. (Landed with S1/S2.) _(size: M)_
  - [x] **S5** — multidimensional values + custom lower bounds + slices `a[m:n]`. Value gained
        `dims`/`lbounds` (codec header already carried them — no format bump); `ARRAY[ARRAY[…],…]`
        stacking (rectangular/`2202E`), `'{{…},{…}}'` + `'[l:u]={…}'` literals, nested-brace + bound-prefix
        `array_out`; subscript node became a list (`a[i][j]` multidim element access, domain `lb..ub`),
        slices (renumber-to-1, clamp, empty→`{}`, NULL-bound→NULL, scalar-in-slice→`1:i`);
        `array_eq`/`array_cmp` count→ndim→dims→lbounds tiebreak; `2202E` registered. All three cores +
        Ruby (golden row 4), `types/array_multidim.test` + `types/array_slice.test`, capabilities
        `types.array_multidim` + `expr.array_slice`. _(size: XL)_
  - [x] **AF1 — the array function/operator surface (the polymorphic foundation)** — the
        `anyarray`/`anyelement` resolution (one type variable `ELEM`, unified by structural equality,
        read back into the `anyarray`/`anyelement` result codes; the `none` non-strict null discipline;
        literal adaptation to the array's element type) + the scalar-function surface: introspection
        (`array_ndims`/`array_length`/`array_lower`/`array_upper`/`cardinality`/`array_dims`) and the
        non-strict builders (`array_append`/`array_prepend`/`array_cat`; multidim append → `22000`,
        incompatible cat → `2202E`). All three cores, oracle-checked (`suites/expr/array_functions.test`),
        capability `func.array`, registry code `22000`. → [array-functions.md](spec/design/array-functions.md)
  - [x] **AF2 — the `||` concatenation operator + the search/edit functions** — `||` as a new
        operator `kind = "concat"` (precedence 37, between comparison 35 and additive 40; the `||`
        token + a `parse_concat` rung; `BinaryOp::Concat` → `resolve_concat`, overload resolution over
        the three concat rows tried cat-first so a bare NULL operand resolves to `array_cat` identity —
        PG) reusing the AF1 builder kernels, plus `array_remove`/`array_replace`/`array_position`/
        `array_positions` (NULL-safe element match; 1-D-only `0A000` for remove/position/positions;
        `array_replace` any-dim; `array_position` returns a SUBSCRIPT, NULL start → `22004`). All three
        cores, oracle-checked (`suites/expr/array_concat_search.test`), registry code `22004`, result
        code `i32[]`. → [array-functions.md §8](spec/design/array-functions.md)
  - [x] **AF3 — `unnest(anyarray)` the set-returning function** — the engine's second FROM-clause
        SRF (after `generate_series`), generalizing the [functions.md §10](spec/design/functions.md)
        SRF machinery to a **polymorphic element-type** output column: a new reserved SRF result
        `set_of_element` (the `anyelement` analogue, bound from the `anyarray` arg → the synthetic
        one-column relation's type) + a per-element row generator (one row per element in flattened
        row-major order; a NULL array or empty array → zero rows; a NULL element → a NULL row;
        multidim flattens, custom lbounds drop). Non-array → `42883`, bare untyped NULL → `42P18`
        (jed posture); each produced element charges one `generated_row` (the `max_cost` ceiling
        bounds a runaway `unnest`, 54P01). FROM-clause position only (the SELECT-list SRF, `LATERAL`,
        `WITH ORDINALITY`, the multi-array form, and array-of-composite elements stay deferred). All
        three cores + Ruby N/A (no format change), oracle-checked (`suites/query/unnest.test`),
        capability `func.unnest`. → [array-functions.md §9](spec/design/array-functions.md)
  - [x] **AF4 — `@>`/`<@`/`&&` the containment/overlap operators** — three polymorphic
        `anyarray <op> anyarray → boolean` operators of a new operator `kind = "containment"`, sharing
        `||`'s precedence rung (37, the PG "any other operator" level; the `concat` parse rung gains
        `@>`/`<@`/`&&` as alternatives, new tokens `@>`/`<@`/`&&` with a lone `@`/`&` → `42601`).
        `a @> b` iff every element of `b` is in `a`; `a && b` iff they share ≥1; `a <@ b` = `b @> a`.
        Match is **STRICT** equality over the **flattened** element multiset (any dimensionality — no
        1-D `0A000`) — a NULL element matches **nothing**, including another NULL (the inverse of the
        AF2 search functions' NOT DISTINCT FROM) — and the operators are strict (NULL whole-array → NULL);
        result is always boolean so an all-untyped-NULL pair is **not** `42P18`. Non-array / element
        mismatch → `42883`. All three cores + per-core unit test, oracle-checked
        (`suites/expr/array_containment.test`), capability `func.array_containment`, `/web` select-page
        live example + e2e. → [array-functions.md §10](spec/design/array-functions.md)
  - [x] **AF5 — `ANY`/`ALL`/`SOME` quantified array comparisons** — `x = ANY(arr)` (the array
        spelling of `IN`) and `x op ALL(arr)` (its universal dual), three-valued over the array's
        flattened elements. A grammar + resolver + evaluator slice with **no new token / catalog row**
        (`ANY`/`SOME`/`ALL` are keywords recognized as a quantifier after a `compare_op`, grammar.md
        §41): an `Expr::Quantified`/`RExpr::Quantified` node whose 3VL fold **reuses the `IN`-list
        membership machinery** (`eq3`/`lt3`/`gt3`; `ANY` is the OR-fold, `ALL` the AND-fold), charging
        per-element like `IN` so `max_cost` bounds it (54P01). The right operand must be an array (a
        non-array side is `42809`); an incomparable element type is `42883`; a bare untyped `NULL`
        operand is `42P18`. All three cores + per-core unit test, oracle-checked
        (`suites/expr/array_quantified.test`), capability `func.array_quantified`, `/web` select-page
        live example + e2e. → [array-functions.md §11](spec/design/array-functions.md)
    - [x] **The subquery quantifier form** `x op ANY/ALL(SELECT …)` — the subquery spelling of `IN`,
          the bridge of AF5 and the §26 subquery machinery. A leading `SELECT` after the quantifier's
          `(` selects it; the body's single column (`42601` if >1) folds through the SAME 3VL
          `quantified_membership` (`ANY` OR-fold, `ALL` AND-fold, no `21000` limit). Uncorrelated →
          folded to a constant-array `Quantified`; correlated → re-executed per outer row. Incomparable
          types `42883`. New `Expr::QuantifiedSubquery`; capability `query.subquery_quantified`. →
          [array-functions.md §11.6](spec/design/array-functions.md)
  - [x] **AF6 — the `VARIADIC` call syntax + variadic overload resolution** — the `make_interval`-era
        follow-on unblocked by the array type, spent on the engine's first VARIADIC built-ins
        `num_nulls`/`num_nonnulls` (count the NULL / non-NULL arguments → i32). A new catalog field
        `variadic=true` marks the last parameter VARIADIC: a call takes EITHER a spread of ≥1 trailing
        args (`num_nulls(1,NULL,3)`, heterogeneous — the variadic element family is `"any"`; zero args
        is `42883`) OR a single array via the `VARIADIC` keyword (`num_nulls(VARIADIC ARRAY[1,NULL,3])`,
        flattened any-dim). One grammar change (the `VARIADIC` keyword before a call's final argument
        only; a non-final/named VARIADIC is `42601`), a `variadic` flag on the FuncCall node, and a new
        `RExpr::Variadic` resolved node. NON-STRICT (`null="none"`): the spread form never returns NULL
        (`num_nulls(NULL)`=1), the VARIADIC-array form returns NULL on a NULL whole-array. A VARIADIC
        non-array / bare-NULL operand is `42804`. One `operator_eval` per call (the count walk
        unmetered, like the introspectors). All three cores + Ruby N/A (no format change), oracle-checked
        (`suites/expr/array_variadic.test`), capability `func.variadic`, `/web` select-page live example
        + e2e. → [array-functions.md §12](spec/design/array-functions.md)
  - [x] **AC1 — array-of-composite elements** — a composite type is now a first-class array element
        type (`items addr[]`): the catalog already framed it (`element_type_code = 14`, array.md §3)
        and the codec/comparison/text-I/O already recursed, so **no `format_version` bump** (still 10).
        Lifts the three `0A000` gates (the `addr[]` column declaration, the `'{…}'::addr[]` literal
        cast, `array_in`'s composite-element coercion) and **fixes the comparison subtlety** the
        feature exposes (array.md §5: a composite element's per-element compare routes through the
        composite *total order* — NULLs-last, definite — not the composite 3VL; so
        `ARRAY[ROW(1,NULL)::addr] = ARRAY[ROW(1,NULL)::addr]` is TRUE and equal-with-NULL arrays sort
        together). Construct (`ARRAY[ROW(…)::addr,…]` / `'{…}'::addr[]`), store/load, `array_out`/
        `array_in` (the two quoting layers nest, array.md §7), compare/`ORDER BY`/`DISTINCT`/`GROUP BY`,
        subscript→`addr`, slice→`addr[]`, `(items[i]).zip`, multidim. New golden
        `array_composite_table.jed` (`rust == go == ts == ruby`); all three cores + Ruby; oracle-checked
        `types/array_composite.test`; capability `types.array_composite`. → [array.md §12](spec/design/array.md)
  - [x] **CMP-ARR-FIELD — a composite type with an array-typed field** (`CREATE TYPE poly AS (name
        text, pts i32[])` — the mirror of AC1): the composite-type catalog entry gains a
        `field_type_code = 15` array field carrying the inline element descriptor (no
        `format_version` bump — still 10; before the field flags byte, where a nested-composite name
        sits), and the value codec / comparison / `record_out` / `record_in` recurse for free (an
        array field's `record_in` token is coerced through `array_in`). The element may itself be a
        composite (the doubly-nested `addr[]` field). `DROP TYPE` dependency tracking + two-pass-load
        validation look through one array level (so an `addr[]` field/column is a `2BP01` dependent).
        Build via `ROW(name, '{…}')` / `ROW(name, ARRAY[…])`; the PG-portable `'(name,"{…}")'::poly`
        cast parses through `record_in`/`array_in`. All three cores + Ruby; new golden
        `composite_array_field_table.jed`; oracle-checked `types/composite_array_field.test`;
        capability `types.composite_array_field`. → [array.md §12](spec/design/array.md), [composite.md §12](spec/design/composite.md)
  - [x] **AF7 — `unnest(composite[])` + the polymorphic array function/operator surface over composite
        elements** — every AF1–AF6 function/operator (`array_append`/`array_cat`/`||`, `@>`/`<@`/`&&`,
        `ANY`/`ALL`, the introspectors, the search/edit functions, `num_nulls` VARIADIC) is oracle-checked
        over a composite element type, and `unnest('{…}'::addr[])` expands a composite array into composite
        rows. Mostly free by construction (the §2 polymorphic resolution unifies a composite element by
        catalog ref; the comparison kernels — `@>`/search-edit — already route through `value_cmp`, the
        composite *total order*). Two pieces needed one-spot-per-core code: (a) `unnest`'s synthetic output
        column types at the bound composite element type (was a scalar-only panic); (b) `x op ANY/ALL(addr[])`
        routes a composite operand pair through the composite total order (definite, NULL fields comparable —
        PG `record_eq`, NOT the bare-`ROW` 3VL `eq3`), while a whole-element NULL still folds to UNKNOWN. All
        three cores + per-core unit tests (the `ARRAY[ROW(…)]`-under-column-context extension), oracle-checked
        (`suites/query/unnest_composite.test`, `suites/expr/array_composite_functions.test`), capability
        `func.array_composite`. → [array-functions.md §13](spec/design/array-functions.md)
  - [ ] _remaining follow-ons (each its own slice + obligations):_ arrays-in-keys
        (`0A000`, encoding authored §8); runtime text→array, `array::text`, and element-wise
        array→array casts. (The subquery quantifier form `op ANY/ALL(SELECT …)` has landed — §11.6.)
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — ✅ **COMPLETE (S0–S6).** The
      **second container axis**, sibling to `array` and sharing ~80% of its foundation, so sequence
      the two together. **The headline implication: this turns the *closed* type enum into an *open*,
      user-defined type system.** Today every type is a variant of a fixed `Copy` enum
      (`ScalarType`), codegen'd from [scalars.toml](spec/types/scalars.toml). A composite type is
      a fact about *a database*: named, created/dropped at runtime, recursive, living in the
      catalog. So `ScalarType` becomes a `Type { Scalar | Composite(catalog-ref) }` threaded
      through parser/resolver/evaluator/codec/comparator/catalog in all three cores, and the
      cross-core contract **shifts in kind**: from "the data table is byte-identical" (scalars) to
      "the *recursive* codec/comparator/NULL-rule/text-I/O is byte-identical" (composites) —
      hand-written per core (§5 forbids codegenning it), policed by new golden fixtures + corpus
      entries (§8). **Subsystems touched:** the type matrix (structural/recursive rules); the
      on-disk catalog + [format.md](spec/fileformat/format.md) (the 1-byte `type_code` can't name a
      user type → a reserved code + a new catalog type-definition section, `format_version` bump,
      new golden); the value codec (a recursive `Value::Composite`, composed with large-values
      overflow + LZ4); comparison/NULL/ordering (field-by-field 3VL; the PG `ROW IS NULL` =
      *all*-fields-NULL gotcha; the TS UTF-8 trap recurses); the grammar/parsers (`CREATE/DROP
      TYPE`, `(expr).field` vs qualified-column ambiguity, `ROW(…)` + bare `(a,b)` constructors, the
      `record_in`/`record_out` text literal); casts; cost units (construct/access/per-field
      compare); and `DROP TYPE` dependency tracking under snapshot isolation. **Decisions to ratify
      spec-first (§8 spirit):** (1) named composites only, or also anonymous `record`; (2) adopt PG's
      all-fields `IS NULL` rule (default yes); (3) **defer composite-as-key `0A000`** (author the
      recursive order-preserving encoding, don't exercise it — the text/decimal-PK precedent); (4)
      skip PG's implicit *table* row-types for now (documented divergence); (5) match `record_in/out`
      quoting or a stricter subset; (6) array-vs-composite sequencing + the shared "containers"
      foundation as one explicit slice. **Path:** NOT a single vertical slice — write
      `spec/design/composite.md` + the **CLAUDE.md §4/§5 revision** (the open-type-system commitment)
      *before* any core touches `ScalarType`, then narrow v1 hard. _(size: XL; §4/§8)_
  - [x] **S0–S2 landed:** `spec/design/composite.md` + the CLAUDE.md §4/§5 open-type-system revision;
        the open `Type { Scalar | Composite }` wrapper threaded through all three cores (a no-op
        refactor); `CREATE TYPE` / `DROP TYPE` + the catalog type registry + **`format_version` 9**
        (kind-tagged catalog entries + a composite-type section + two-pass acyclic load), persisted
        byte-identically across rust/go/ts/ruby with new goldens (`composite_type_table.jed`,
        `nested_composite_table.jed`); error `2BP01`; the `types.composite` capability +
        `ddl/create_type.test`. Nested composites + dependency tracking work.
  - [x] **S3 landed:** a storable composite **column** (the `0A000` lifted) + the recursive value
        codec (null bitmap + present-field bodies, [format.md](spec/fileformat/format.md) *Value
        codec*) threaded through the codec seam (`ColType`) in all three cores; the `ROW(…)`
        constructor (parser/AST/eval) in expression + INSERT VALUES position; INSERT/SELECT
        round-trip; `record_out` rendering (PG field quoting); structural `eq3`/`lt3`/`gt3`. The two
        composite goldens now carry composite-column **values** (rust/go/ts/ruby byte-identical), and
        `types/composite.test` is oracle-shaped. **S3 narrowings (relaxed later):** composite
        comparison in `WHERE`, `INSERT … SELECT` into a composite column, and `UPDATE` of one are
        `0A000`; `DEFAULT` on a composite column is `0A000`.
  - [x] **S4 landed:** field access `(expr).field` / `(expr).*` — the **parens-required** `.field`/
        `.*` postfix operator (chains with `::` and itself), the resolver field lookup
        (case-insensitive; unknown field `42703`, non-composite base `42809`), and `(expr).*`
        projection-list expansion. The differential oracle **corrected** the planned bare-`col.field`
        fallback: live PG requires parens (`home.zip` → `42P01`; field access is `(home).zip`), so
        jed matches PG (no fallback). No on-disk format change.
  - [x] **S5 landed:** resolver-level element-wise comparison / ordering — `classify_comparable`
        lifted (same-arity, field-comparable composites OK; `42804` otherwise), the **non-recursive**
        all-fields `IS NULL`/`IS NOT NULL` rule (the differential oracle corrected the recursive
        assumption — a composite-valued field counts as present), the `ORDER BY` lexicographic
        total-order arm, and DISTINCT/GROUP BY composite keys (the S3 value Hash/Eq). S5 corpus rows
        PG-verified; all three cores green (108/0/0); no format change.
  - [x] **S6 landed:** PG-exact `record_out` (`"`→`""`, `\`→`\\` doubling — the oracle corrected the
        S3 `\"` rendering) + `record_in` (`value::parse_record_tokens` + `coerce_string_to_composite`)
        wired into the `'(…)'::type` cast and the `type '(…)'` typed literal (string-literal →
        composite; runtime text→composite, `composite::text`, and `ROW(…)::type` stay `0A000`). The
        oracle check is **green**: `rake corpus:check` regenerates `types/composite.test`
        byte-identically from live PG (two documented comparison-error-code overrides — jed `42804`
        vs PG `42883`/`42601`). All three cores green (108/0/0); no format change.
  - **Still narrowed (relaxable later):** `INSERT … SELECT` / `UPDATE` of a composite column;
        composite `PRIMARY KEY` / index / `UNIQUE` (`0A000` — key encoding authored, unexercised);
        `DEFAULT` on a composite column; runtime non-literal text→composite + `composite::text` +
        anonymous `ROW(…)::type` casts; the nested `ROW(ROW(…),…)`-into-column constructor (a jed
        extension PG rejects — in unit tests, not the PG-oracle corpus).

---

## Phase 4 — Relational depth + constraints

> The meaty planner/executor work and the rest of the integrity story.

- [x] **`JOIN` — multi-table FROM + `INNER`/`CROSS`** — left-deep chain, table aliases, qualified
      column refs, a scope resolver baking a flat index into `Column`, a left-deep nested-loop
      executor; ambiguity `42702`, dup alias `42712`. → [grammar.md §15](spec/design/grammar.md)
  - [x] **Outer joins — `LEFT`/`RIGHT`/`FULL [OUTER] JOIN`** — executor-only NULL-extension branch
        as planned; WHERE-downgrades-to-inner falls out free.
    - [ ] _follow-on:_ `USING` / `NATURAL` / comma-`FROM` / `t.*`.
- [x] **Subqueries (uncorrelated)** — scalar `(SELECT …)`, `x [NOT] IN (SELECT …)`, `[NOT] EXISTS`
      via plan-time folding (executed once, replaced by a constant — the per-row evaluator
      untouched); `21000` cardinality, `42601` >1 col. → [grammar.md §26](spec/design/grammar.md)
  - [x] **Correlated subqueries** — split `run_select` into resolve (`plan_query`) + execute; a
        scope chain (`Local`/`Outer{level,index}`), an `EvalEnv` row stack; uncorrelated still
        folded once.
    - [ ] _follow-on:_ a correlated `GROUP BY` / `ORDER BY` key (`0A000`, degenerate).
  - [x] **Subqueries in UPDATE / DELETE** — `allow_subquery` on the single scope; pre-statement
        snapshot preserved.
  - [x] **`$N` inside a subquery** — one `ParamTypes` threads the whole plan tree; the lone gap
        (a `$N` typed only by the enclosing query) raises `42P18` (documented divergence).
  - [x] **Derived tables** (`FROM (SELECT …) AS t`) — a parenthesized subquery as a FROM relation,
        the parser surface over the CTE slice's inline seam: a derived table is mechanically an
        anonymous, always-inlined single-reference CTE (no materialize path, no `cte_scan_row`).
        Body planned `parent = None` (non-correlated, no LATERAL) but inheriting CTE bindings;
        **optional** alias (matching PG 18, which relaxed the mandatory-alias rule — an unaliased
        derived table has no qualifier), optional column-rename list (`42P10`), explicit-label
        collision `42712`, leading-`(`-not-`SELECT` `42601`, depth counts toward `54001`. New
        `query.derived_table` capability. → [grammar.md §42](spec/design/grammar.md)
    - [x] **A `VALUES` body** (`FROM (VALUES (1),(2)) AS v(x)`) — a parenthesized `VALUES` list as a
          FROM relation, a computed relation of literal rows reusing the derived-table seam. Values
          are **general constant expressions** (richer than the literal-only `INSERT … VALUES` slot —
          the maintainer's call, matching PG): non-`LATERAL` (`parent = None`), so a column ref is
          `42703`, an aggregate `42803`, a bare `$N` `42P18`. Rows must share arity (`42601`); default
          column names `column1…`; per-column type unification across rows like a set op (`42804`).
          A leading `(` + `VALUES` selects it; a trailing `ORDER BY`/`LIMIT` on the body is `42601`
          (deferred). New `query.values` capability. → [grammar.md §42](spec/design/grammar.md)
    - [x] **`LATERAL`** — ✅ a FROM item (LATERAL `(SELECT…)`/`(VALUES…)` derived table, or an
          implicitly-lateral table function) whose body / args reference the EARLIER FROM relations, a
          dependent join re-evaluated per left-hand row, reusing the correlated-subquery machinery.
          Reached via `[CROSS|INNER|LEFT] JOIN LATERAL`; `RIGHT`/`FULL` to a correlated lateral is
          `42P10`; SRFs are implicitly lateral (lifting the §35 narrowing). All three cores +
          `query.lateral` + `suites/joins/lateral.test`. → [grammar.md §44](spec/design/grammar.md)
    - [ ] _follow-on:_ a **parenthesized-join FROM** (`FROM (a JOIN b ON …)`); a trailing **`ORDER
          BY`/`LIMIT` on a VALUES body**; **comma-`FROM`** (`FROM t, LATERAL (…)`) — until it lands,
          LATERAL is reached only through explicit `JOIN` syntax.
  - [x] **`ANY` / `ALL` over a subquery** — `x op ANY/ALL(SELECT …)`, the subquery spelling of `IN`;
        see the AF5 sub-item above and [array-functions.md §11.6](spec/design/array-functions.md).
  - [ ] **Subqueries — remaining seams:** subqueries in an **`INSERT ... VALUES`** slot (blocked on
        VALUES holding a general expression); **row-valued** subqueries. _(size: S)_
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — a query-expression
      precedence tree (INTERSECT binds tighter), full-PG per-column type unification, NULL-safe
      multiset semantics, trailing ORDER BY by output-column name. → [grammar.md §25](spec/design/grammar.md)
  - [ ] _follow-on:_ parenthesized operands `(SELECT …) UNION …`; ORDER BY/LIMIT inside an operand;
        ORDER BY ordinals; a set op in an `INSERT … SELECT` source.
- [x] **Common table expressions (`WITH`)** — `WITH name [(cols)] AS [NOT] MATERIALIZED (query)
      [, …] <query>`: named subqueries visible as relations in the statement's FROM (and to later
      CTEs in the same WITH list — forward-only). A CTE is a **named derived table**: the scope
      machinery now serves relations that aren't catalog tables (the synthetic-relation seam the
      SRF path opened, generalized to a planned body), so the inline path also lands the
      derived-table executor internally. Evaluation follows **PostgreSQL's hybrid rule** — INLINE a
      single-reference CTE, MATERIALIZE a multi-reference / `MATERIALIZED` one, the new
      `cte_scan_row` cost unit metering a buffer scan (the deterministic cost contract, cost.md §3).
      A CTE name shadows a same-named catalog table except inside its own body; a duplicate name is
      `42712`, a self/forward reference `42P01`, too many rename aliases `42P10`, `WITH RECURSIVE`
      `0A000`. → [cte.md](spec/design/cte.md)
  - [ ] _follow-on:_ **`WITH RECURSIVE`** (the iterate-to-fixpoint executor + a termination story —
        the `54P01` cost ceiling does real work there); **data-modifying CTEs**
        (`WITH x AS (INSERT … RETURNING …)`); **`WITH` on UPDATE/DELETE**; a **nested `WITH`** inside
        a subquery or CTE body (top-level only this slice); and the inline derived-table **syntax**
        `FROM (SELECT …) AS t` (the executor seam landed; only the parser surface remains).
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
      ordered B-tree, via a type-generic operator-class seam (extract-terms / extract-query /
      consistent). This slice: the **`array_ops`** opclass over a single integer-element array
      column (`int16[]`/`int32[]`/`int64[]`), accelerating **`@>`** and **`&&`** only; one entry
      per distinct non-NULL element (`encode(elem) ‖ storage-key`, empty payload); the planner
      gathers candidates by posting-list intersection (`@>`) / union (`&&`) with the predicate as
      the residual filter; `format_version` 12 adds a per-index `index_kind` byte; a new
      `gin_entry` cost unit. Spec + corpus authored (G0): → [gin.md](spec/design/gin.md),
      `suites/ddl/create_gin_index.test`, `suites/query/gin_scan.test`. _(size: L; deps: secondary
      indexes ✅, arrays ✅, `@>`/`&&` ✅)_
  - [x] _G1:_ grammar `USING` + `IndexKind` + the `index_kind` byte + the `gin_array_table.jed`
        golden (byte-identical rust == go == ts == ruby), term extraction + N-entries-per-row
        maintenance — all three cores + the Ruby reference (the index builds & round-trips on disk;
        `create_gin_index.test` green on every core; queries don't use it yet — that's G2).
  - [x] _G2:_ the planner GIN bound + multi-term gather + cost (`gin_entry`), `gin_scan.test` cost
        assertions, the `gin` NoREC scenario (`scripts/norec_gen.rb`), a `bench/` GIN workload, and
        the `/web` docs + the oracle-override ledger entries for the deferred-narrowing DDL records.
  - [x] _follow-on — `= ANY(col)` membership acceleration:_ `c = ANY(gin_col)` (the array spelling
        of membership) over a GIN-indexed array column bounds the scan via a **single-term `@>`
        reduction** (`c = ANY(col)` ⇔ `col @> ARRAY[c]`): a third `GinStrategy` (`Member`) whose
        query operand is the scalar `c`, gathered as one posting list, original `= ANY` predicate
        kept as the residual filter (same rows as the full scan, lower cost). A NULL `c` (typed
        `NULL::i32`) is a provably-empty bound; an out-of-element-range `c` is rejected `22003` at
        resolve before the bound (jed coerces `c` to the element type — a divergence from PG, which
        full-scans `= ANY(array)` and returns empty). All three cores + capability
        `query.gin_any_eq` + `suites/query/gin_any_eq.test` (cost-asserted, oracle-checked) + the
        `gin_any` NoREC scenario + `/web` Indexes page + e2e. → [gin.md §6](spec/design/gin.md)
  - [x] _follow-on — array `=` acceleration:_ `gin_col = const` (exact array equality, commutative)
        over a GIN-indexed array column bounds the scan via the **`@> distinct(const)` superset
        gather + residual `=`** (equal arrays have identical element multisets, so `col = const` ⟹
        `col @> const` — the `@>` intersection is a sound superset, made exact by the residual `=`):
        a fourth `GinStrategy` (`Equal`). Two shapes part from `@>`: a NULL **element** does NOT empty
        the bound (`col = ARRAY[1,NULL]` matches a `{1,NULL}` row via the `@> {1}` bound), and a
        `const` with no non-NULL element (`'{}'`/all-NULL) **falls back to the full scan** (its
        matching rows carry no index terms), not a provably-empty bound. **Matches PG** (its
        `array_ops` GIN has the `=` strategy `GinEqualStrategy 4`, also lossy→recheck). All three
        cores + capability `query.gin_array_eq` + `suites/query/gin_array_eq.test` (cost-asserted,
        oracle-checked) + the `gin_eq` NoREC scenario (`= Q` vs `NOT(<> Q)`) + a `gin_array_eq`
        bench + `/web` Indexes page + e2e. → [gin.md §6](spec/design/gin.md)
  - [x] _follow-on — GIN bounds for UPDATE/DELETE scans:_ a mutation whose `WHERE` has a
        GIN-accelerable conjunct (`@>`/`&&`/`= ANY`/`=`) now bounds its **target-row scan** through
        the GIN index instead of full-scanning (PK-then-GIN-then-full; the ordered-index equality
        bound stays SELECT-only, a separate follow-on). Refactored `gin_bound_rows` to return
        `(storage_key, row)` pairs — the candidate set IS the keys — so the mutation can rewrite/remove
        them; a shared `detect_gin_bound` helper feeds both the SELECT planner and the mutation scan.
        The bound is over the pre-mutation index state and the array column is in the `WHERE` (so
        resolved), so GIN-entry maintenance stays correct; end state + RETURNING rows identical to the
        full scan. **Matches PG** (it uses its array GIN index for UPDATE/DELETE too). All three cores
        + capability `query.gin_mutation` + `suites/query/gin_mutation.test` (cost-asserted across all
        four strategies + the `@> '{}'` fallback + a miss, oracle-checked) + the `gin_mut` NoREC
        scenario (index-bound mutation vs `<@` full-scan mutation, same end state) + a `gin_delete`
        write-rollback bench + `/web` Indexes page. → [gin.md §6](spec/design/gin.md)
  - [x] _follow-on — non-integer (fixed-width key-encodable) element types:_ a `USING gin` index, and
        every GIN-bounded scan (`@>`/`&&`/`= ANY`/`=` and the GIN-bounded UPDATE/DELETE), now admit an
        array column whose element type is any of the engine's keyable scalars beyond the integers —
        `uuid[]`, `date[]`, `timestamp[]`, `timestamptz[]`, `boolean[]` (the same set a PK / ordered-index
        key column accepts). A GIN term IS the element's order-preserving key encoding, so the inverted
        core was unchanged: only the CREATE INDEX gate (a shared `is_gin_element_type` predicate) and the
        per-element term encoder generalized from `encode_int` to the shared `encode_key_value` — the
        bytes/rows/cost are the integer case's over a wider element domain. All three cores + capability
        `query.gin_element_types` + `suites/query/gin_element_types.test` (the four strategies + a
        GIN-bounded DELETE over each new type, cost-asserted, oracle-checked) + the `gin_uuid_table.jed`
        byte golden (rust==go==ts==ruby) + `/web` Indexes page. No `format_version` bump (uuid/date/
        timestamp key encodings are already on disk). → [gin.md §3/§4](spec/design/gin.md)
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
- [x] **Sequences** (`CREATE SEQUENCE` / `nextval` / `currval`) — ✅ **landed (S0–S5)**: the PostgreSQL
      sequence object as a third catalog-object kind (after tables + composite types): a named, persisted, monotonic
      **i64** generator in `Snapshot.sequences`, advanced by `nextval('s')` and read by
      `currval('s')` (session-local). **The defining decision — `nextval` is TRANSACTIONAL** (rolls
      back with the txn), a deliberate PG divergence already mandated by
      [determinism.md §5](spec/design/determinism.md) ("do not exempt" the counter): jed is
      single-writer, so PG's non-transactional gap optimization is unneeded and would force a seam +
      determinism-ledger exemption. New `entry_kind = 2` catalog entry, **`format_version` 12**, a
      `sequence_advance` cost unit; `nextval`/`setval` make a statement a write (`25006` in a
      read-only txn). → [sequences.md](spec/design/sequences.md) _(size: XL; §4/§8)_
  - [x] **S0** — `spec/design/sequences.md` + the error registrations (`2200H`/`55000`) + the §5
        transactional-divergence record + this TODO touch. Decisions ratified spec-first.
  - [x] **S1** — `CREATE`/`DROP SEQUENCE` (full option grammar) + the `sequences` catalog map +
        `format_version` 12 + the `sequence_table.jed` golden (`rust == go == ts == ruby`) +
        `nextval` + `currval` + the `sequence_advance` unit + write-path detection + read-only
        `25006` + corpus (`ddl/sequence.test`, `expr/sequence_value.test`) + capabilities
        `ddl.sequence`/`func.sequence`. The "it's alive" slice. _(size: L)_
  - [x] **S2** — `setval(s,n[,is_called])` + `lastval()` (the `session_last` source) + `ALTER
        SEQUENCE [IF EXISTS] s RESTART [WITH n]` (the first `ALTER` action) + corpus coverage of
        `CYCLE` wraparound and the bound errors (`22003` setval / `22023` RESTART). `setval`/`ALTER`
        reuse the `nextval` write-path + transactional-rollback machinery; with `setval` available
        the corpus sets a known state in one statement and asserts directly. _(size: M)_
  - [x] **S3** — `serial` / `bigserial` / `smallserial` (aliases `serial4`/`serial8`/`serial2`)
        CREATE-TABLE column pseudo-types: sugar for an `i32`/`i64`/`i16` column that is `NOT NULL`
        with a `DEFAULT nextval(...)` backed by a newly-created **owned** sequence
        (`<table>_<col>_seq`, numeric-suffix collision resolution). The `OWNED BY` link is persisted
        (**`format_version` 14** — a `has_owner` flag bit + trailing owner table/ordinal on the
        sequence entry, new `serial_table.jed` golden `rust == go == ts == ruby`), so `DROP TABLE`
        auto-drops the owned sequence (across a reopen) and `DROP SEQUENCE` of an owned sequence is
        `2BP01`; an explicit `DEFAULT` on a serial column is `42601`. Owned sequences are
        `bigint`-flavored for all three (the `AS type` deferral — a documented divergence); the
        column type bounds stored values. All three cores + Ruby; `ddl/serial.test`; capability
        `ddl.serial`. → [sequences.md §12](spec/design/sequences.md) _(size: M–L)_
  - [x] **S4** — `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]` columns + the
        `OVERRIDING { SYSTEM | USER } VALUE` INSERT clause (the SQL-standard identity surface). Reuses
        S3's owned-sequence + `nextval`-default + `NOT NULL` desugaring, adding only two persisted
        column flag bits (**`format_version` 15** — bit 4 `is_identity`, bit 5 `identity_always`), the
        `identity_table.jed` golden (`rust == go == ts == ruby`), the `428C9 generated_always` error,
        the `i16`/`i32`/`i64`-only type gate (`22023`), the `CREATE TABLE` conflicts (`42601`), and the
        INSERT/UPDATE value gating. All three cores + Ruby; `ddl/identity.test`; capability
        `ddl.identity`. → [sequences.md §13](spec/design/sequences.md) _(size: L)_
  - [x] **S5** — the `AS { smallint | integer | bigint }` sequence data type (an order-free `CREATE
        SEQUENCE` option) → the type sets the default + validated `MINVALUE`/`MAXVALUE`; `serial`
        follows the pseudo-type and a `GENERATED AS IDENTITY` column follows its column type (both
        auto-wiring the owned sequence's type). **Closes the bigint-flavored divergence** (the old
        decisions 3/9/11 — a `smallserial` / `smallint` identity sequence is now bounded to
        `[1, 32767]`, trapping `2200H` like PG) and corrects the bigint descending default min to
        `i64::MIN`. A non-integer `AS` type or an explicit bound outside the type range is `22023`; an
        `AS` clause inside an identity column's `( … )` options is `42601`. The type is **not
        persisted** (reducible to the MIN/MAX bounds), so **no `format_version` change** — only the
        `serial_table.jed` / `identity_table.jed` goldens move (`MAXVALUE 2147483647`). All three
        cores + Ruby; `ddl/sequence_as_type.test`; capability `ddl.sequence_as_type`.
        → [sequences.md §14](spec/design/sequences.md) _(size: M)_
  - [x] **S6** — the `ALTER SEQUENCE` **definition-changing option set** (the order-free `CREATE`
        options minus `AS`, plus an interleavable `RESTART`) **+ `RENAME TO`**. Re-runs PG
        `init_params` with `isInit = false` (only written options change; `last_value`/`is_called`
        preserved unless `RESTART`); the two post-edit cross-checks (`START`, then the preserved
        `last_value`), strict `MINVALUE < MAXVALUE` (also corrects the `CREATE` path, which previously
        allowed `==`). `RENAME TO` moves the catalog key (`42P07` collision, same name included) and
        rewrites an **owned** sequence's owning-column `nextval` default so a later `INSERT` still
        works. A bare `ALTER SEQUENCE s` is `42601`; `AS type`/`OWNED BY`/`OWNER TO`/`SET …` stay
        `0A000`. **No `format_version` change** (no golden moves). All three cores; `ddl/alter_sequence.test`;
        capability `ddl.alter_sequence`. → [sequences.md §15](spec/design/sequences.md) _(size: M)_
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET …
      [WHERE …] }`: a candidate row that would violate a UNIQUE/PRIMARY KEY constraint takes the
      conflict action instead of trapping `23505`. DO NOTHING skips it; DO UPDATE updates the
      existing conflicting row, with the proposed row exposed as the qualifier-only `excluded`
      pseudo-relation. The arbiter target is a column SET matched order-independently against a
      unique index / the PK (no match `42P10`), or `ON CONSTRAINT name` (a unique-index name or the
      synthesized `<table>_pkey`; miss `42704`); DO UPDATE requires a target (`42601` without one),
      DO NOTHING may omit it (any conflict skipped). A non-arbiter conflict still traps `23505`; two
      proposed rows sharing the arbiter key are `21000` under DO UPDATE, skipped under DO NOTHING.
      Two-phase / all-or-nothing with sequential planning; RETURNING projects the affected
      (inserted + updated) rows. No new error code (reuses existing), no on-disk format change. All
      three cores + capability `dml.insert_on_conflict` + `dml/insert_on_conflict.test` (oracle-clean)
      + per-core divergence/introspection tests. → [upsert.md](spec/design/upsert.md), grammar.md §46
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT` (with the `UPDATE` `SET = DEFAULT` follow-on);
        `INSERT INTO t AS alias` (the existing row is referenced by the table name today); the
        partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing
        the DO UPDATE PK-column assignment (`0A000`) with the UPDATE re-keying follow-on. → [upsert.md §10](spec/design/upsert.md)
- [ ] **Relax the UPDATE narrowings** — allow assigning a `PRIMARY KEY` column (currently
      `0A000`; means the storage key can change). Documented as relaxable (§11 step 6).
      _(size: M; deps: transactions for clean re-keying)_

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
- [x] **P5.4 — cross-core concurrency conformance, Layer 1 (the schedule format)** — closes the gap that
      P5.3's concurrency was tested only by hand-mirrored per-core suites (outside the differential net).
      A `# format: concurrency` `.test` file is an explicit total order over named read/write sessions on
      one `SharedDb`; deterministic because jed read results depend only on commit order + pin-points, not
      timing. New caps `txn.shared`/`txn.read_handle`/`txn.watermark`; `suites/concurrency/snapshot_isolation.test`
      pins snapshot isolation, cross-handle visibility, 25006-no-poison, and the `oldest_live_txid` watermark.
      **Runner landed in all three cores** (`impl/{go,rust,ts}` conformance harnesses, stepped-sequential =
      the canonical result); **Go + Rust also run the stepped-threaded mode** (one goroutine/OS-thread per
      session under a turn token) under the race detector via `rake concurrency:race`.
      → [concurrency-testing.md](spec/design/concurrency-testing.md)
- [x] **P5.4 (Layer 2) — the write-gate `blocks` annotation** — `open <sid> write blocks` asserts the
      held single-writer gate, queuing the writer-open until the holder commits/rolls back (the
      equivalent serial order). New cap `txn.gate_blocking`; `suites/concurrency/gate_blocking.test`.
      **Landed in all three cores** — all defer the queued open to the gate-releasing step (the canonical,
      timing-free result, so the TS core models the block without truly blocking); **Go + Rust additionally
      park the queued writer's thread inside the real `write()` on the held gate under the race detector
      (`rake concurrency:race`)**, verifying the open had not returned before the release — the one
      concurrency path the sequential walk never exercises. At most one writer blocked at a time
      (single-writer model). Three schedules now (`snapshot_isolation`/`watermark_refcount`/`gate_blocking`).
      → [concurrency-testing.md §5](spec/design/concurrency-testing.md)
- [x] **P5.4 (Layer 3) — the parallelism-stress format** — `stress/*.stress.toml` + `rake stress`,
      bench-family (OUTSIDE `rake ci`, timing-nondeterministic but answer-checked). A workload of
      concurrent writers + readers with NO fixed order; correctness by INVARIANTS, not a transcript:
      a per-snapshot invariant (`sum(bal)==1000` on every reader snapshot — torn-read / isolation /
      watermark bug → wrong sum), a confluent final state (exact rows + the lost-update check), and a
      cross-core final-state checksum that must agree across cores regardless of mode. One stress
      binary per core in the `bench/` modules (reusing the splitmix64 PRNG + FNV-1a answer checksum,
      no new dependency): **Go under `-race`** (one goroutine per worker), **Rust over real OS
      threads** (Send + Sync), **TS via a seeded-sequential interleaver** (the single-thread fallback —
      deterministic given the seed, never truly blocks). First file: `stress/balance_transfer.stress.toml`;
      all three cores agree on the checksum. Highest payoff once file-backed sharing is wired.
      → [concurrency-testing.md §6](spec/design/concurrency-testing.md)

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
        spills sorted runs + k-way merges, reproducing the in-memory stable sort byte-for-byte; the
        single-table path fuses scan→filter→Sorter. Result- and cost-invariant; stdlib temp files only.
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
            `fsync`→`fdatasync`, so steady-state commits overwrite already-allocated space
            metadata-free: ~9.0ms → ~2.5–3.1ms p50 (~2.7×), identical cross-core checksums. Batched/
            group commit under relaxed `synchronous` remains orthogonal. → [pager.md §7](spec/design/pager.md)
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
      `std::fs`/`os` in the per-core `Pager`. Remaining work:
  - [x] **`BlockStore` extraction** — the file backing lifted into a `FileBlockStore` behind the
        five-method interface; the pager composes it + keeps the policy. The in-memory path was
        deliberately left separate (not a behavior-preserving refactor). → [hosts.md §3/§7](spec/design/hosts.md)
  - [x] **Browser/OPFS host** (`FileSystemSyncAccessHandle`) — TS-only `OpfsBlockStore` mapping the
        five methods onto `read`/`write`/`truncate`/`getSize`/`flush`, with the engine in a Web Worker
        driven by an async client (`src/browser/`). Confirmed **file-host parity** in Node against the
        goldens (`tests/opfs_parity.test.ts`); gated real-browser e2e via Vite + Playwright
        (`npm run test:browser`, needs `npx playwright install chromium`). Making the TS engine
        browser-bundle-clean lifted its `node:*` imports behind seams (`fileblockstore.ts` split, a
        `SpillSink` seam + `spillfile.ts`, Web Crypto entropy default). Deferred follow-ons: OPFS
        disk-spill, the e2e in CI. → [hosts.md §5](spec/design/hosts.md) _(§9)_
- [x] **Cost ceiling (`max_cost`) + deterministic abort** — a handle `max_cost` setting aborts a
      statement with `54P01` the instant accrued cost reaches it, via `Meter::guard()` at the
      unbounded-work points; the `# max_cost:` corpus directive pins it. → [cost.md §6](spec/design/cost.md) _(§13)_
  - [x] **Bound expression-nesting depth** (native-stack safety for untrusted input) — a fixed
        `MAX_EXPR_DEPTH = 256` checked in the recursive-descent parser (one shared counter
        incremented at every AST level: binary-chain step, unary, postfix, sub-expression re-entry,
        nested subquery, set-op branch), aborting with `54001 statement_too_complex` BEFORE
        deeply-nested input (`1+1+…`, nested parens/`ARRAY`/subscripts/subqueries/`UNION`) can
        overflow the parser/resolve/eval stack — the gap the `54P01` cost ceiling structurally
        cannot catch (it strikes before metering). Bounding at the parser keeps every downstream
        walk safe with no extra guard sites. A deterministic, cross-core-identical constant (a
        documented divergence from PG's runtime `check_stack_depth` probe — chosen for the weakest
        core's native stack, the TS/Node default, which overflows at ~547 nested subqueries).
        All three cores + `resource.depth_limit` capability + `resource/depth_limit.test`.
        → [cost.md §7](spec/design/cost.md) _(§13)_
- [x] **The `jed` CLI** — a full-screen TUI client (Rust + ratatui/crossterm/tui-textarea, the
      §14-approved deps) + a plain script mode (`-c`/`-f`/stdin; aligned/csv/json). A host program,
      not a core. → [cli.md](spec/design/cli.md)
- [x] **Affected-row counts in `Outcome`** — DML without RETURNING reports rows touched (PG command
      tags), an additive `Outcome` field in all 3 cores. → [api.md §4](spec/design/api.md)
- [x] **CLI follow-ons** — editor autocomplete + syntax highlighting, CSV import/export, `--dump`
      SQL export, `-o` redirection, `box`/`markdown` formats, `--readonly` open mode. → [cli.md §8](spec/design/cli.md)
- [ ] **Sessions — the configured host context** — un-fuse `Database` (storage identity) from a
      first-class **`Session`** (the configured, capability-bearing context a host runs statements
      through), the explicit home for the settings the handle conflated + the new host controls.
      Spec authored: → [session.md](spec/design/session.md). Sequenced slices (each its own vertical
      slice + corpus, §10):
  - [x] **S1 — session concept + the one stateful default session** — ✅ **landed (all 3 cores).** A
        `Session` type (Rust struct / Go struct / TS class) holds the per-connection state — the
        relocated settings (`max_cost`/`max_sql_length`/`work_mem` + the entropy/clock seam), the open
        transaction, and the `currval`/`lastval` session state — and `Database` owns one as its
        long-lived **stateful default session** (an open `BEGIN`, meters persist across calls —
        PG/SQLite connection model, §2.1). The **transaction state machine** is explicit on the
        session (`Idle`/`Open`/`Failed` = `TxStatus`/`db.status()`, §2.2); the separate `Transaction`
        object **collapses** to session state + RAII sugar. `db.session(opts)`/`NewSession`/`newSession`
        mints additional independent sessions that share committed storage and run **sequentially via
        a swap** (`Session::execute`/`query`/`view`/`update`). State ownership: committed data on
        `Database`, session state on `Session`. Near-pure refactor — corpus + all suites unchanged
        (162/0 ×3, NoREC 660/660), per-core `session` tests added. _(§2)_
  - [x] **S2 — multi-statement splitter + `execute_script`** — ✅ **landed (all 3 cores).** A
        **library-level** (no `Session`/`Database`) lazy **`split_statements(sql)`** iterator
        (`SplitStatements`/`StatementSpan`; top-level core export / parser surface; an O(n) lexer-level
        boundary scan respecting string literals, dollar-quoted strings, and line/block comments,
        yielding one statement span at a time, buffering nothing) — the host loops it through the
        normal single-statement path, so all existing bounds (`max_sql_length`/`54001`/`max_cost`/
        privileges/cursor) apply for free. Plus the thin **`db.execute_script(sql)`** /
        **`session.execute_script`** convenience: split + run-each + **discard rows** + one implicit
        transaction when `Idle` (all-or-nothing; **joins** an already-`Open` transaction), returning
        the `O(1)` `ScriptSummary { statements_run, rows_affected_total, cost }`. NOT a buffering
        `Vec<Outcome>` batch (that would be an unbounded buffer, violating §13). Both are **host-API
        surface**, so **per-core unit tested** (the single-statement corpus can call neither, §10):
        `split.rs`/`split_test.go`/`split.test.ts` (boundary correctness) + `execute_script` tests
        (atomicity / join / counts). **v1 narrowing:** in-script transaction control
        (`BEGIN`/`COMMIT`/`ROLLBACK`) is **`0A000`** — the implicit wrapper owns the boundary;
        PG-simple-query partitioning is deferred (session.md §11). No new error code, no format
        change, no capability flag. → [session.md §4](spec/design/session.md) _(§4)_
  - [x] **S3 — privileges (the GRANT/REVOKE model)** — ✅ **landed (all 3 cores).** Per-table
        `SELECT`/`INSERT`/`UPDATE`/`DELETE` + per-function `EXECUTE`, expressed as a session
        `default_privileges` set (granted to all tables — replaces the read-only/read-write boolean)
        plus per-object `grant`/`revoke` deltas (revoke wins), and an `allow_ddl` gate; collected by
        an exhaustive per-statement AST walk (the `seq_mutator`-walk precedent) and enforced at the
        executor's `dispatch_stmt` seam with **`42501 insufficient_privilege`** — DDL by `allow_ddl`,
        a table privilege only for a name that **resolves to an existing catalog table** (a missing
        table stays `42P01`; CTE/derived labels skipped), a function by `EXECUTE`. A fully-permissive
        session (the default) skips the walk, so the common path is untouched (NOT RBAC — the host
        holds the grants, §3/§13; the physical read-only file / `READ ONLY` txn `25006` gate stays
        orthogonal at the Database/txn layer). New module `privileges.{rs,go,ts}` (`Privilege` /
        `PrivilegeSet` bitset / `Privileges` envelope); registers `42501` in the registry; **no
        on-disk format change** (the envelope is session state, never persisted). The SQL-observable
        `42501` is **cross-core corpus-tested** (`suites/session/privileges.test`, jed-specific so not
        oracle-checked), the host-API surface per-core unit tested (`privileges.rs`/`privileges_test.go`/
        `privileges.test.ts`). Capabilities `session.privileges` / `session.allow_ddl`;
        `# default_privileges:` / `# grant:` / `# revoke:` / `# allow_ddl:` directives. **v1 narrowing:**
        function `EXECUTE` defaults on (deny-list via `revoke`); the allow-list (default-off) is
        deferred (§11). Green: conformance 163/0 ×3, full suites. _(§5.3/§13)_
  - [x] **S4 — session lifetime cost budget** — ✅ **landed (all 3 cores).** A per-session cumulative
        cost meter aborting with **`54P02 session_cost_limit_exceeded`** (new `P`-subclass code) the
        instant the session's running total reaches `lifetime_max_cost`. Sibling to the per-statement
        `max_cost`/`54P01`: a statement aborts at whichever ceiling it reaches first (the per-statement
        ceiling wins an exact tie). Implemented by threading the session's cumulative total through the
        per-statement `Meter` (a shared `Rc<Cell<i64>>` / `*int64` / object reference the meter
        live-charges into), so **partial cost of an aborted statement counts automatically** and the
        cumulative is **session state, not snapshot state** — it does NOT roll back with a `ROLLBACK`.
        Once spent, every further statement is rejected `54P02` at **admission** (checked before
        privileges, before any work). Host API: `db`/`session`.`{set_lifetime_max_cost, lifetime_cost,
        lifetime_max_cost}` (the cumulative gauge + the budget setter). Registers `54P02`; capability
        `session.lifetime_cost`. The SQL-observable `54P02` (in-flight abort, admission rejection, and
        the `54P01`-vs-`54P02` precedence) is **cross-core corpus-tested** via a sticky
        `# lifetime_max_cost: N` directive + an ordered statement sequence on the one session
        (`suites/session/lifetime_cost.test`, jed-specific so not oracle-checked); the gauge + setters +
        no-rollback + partial-cost host-API surface is per-core unit tested. **No on-disk format
        change** (the cumulative is session state, never persisted). _(§5.4/§13)_
  - [ ] **S5 — session variables (v1)** — a string→string GUC map, host get/set + `current_setting()`
        read; namespaced custom vars; `# set:` directive. (`SET LOCAL` / full SQL `SET`/`SHOW` /
        `set_config()` deferred.) Capability `session.variables`. _(size: M; §6.1)_
  - [ ] **S6 — session time zone slot** — the built-in `time_zone` var (default **`UTC`**, fixed
        offsets only, named zones `0A000`), injected not OS-read (determinism, §6); the
        `# timezone:` directive. Forward-looking infra — the consuming `timestamptz→date`/`AT TIME
        ZONE` cast is a separate Phase-3 type slice. Capability `session.timezone`. _(size: S; §6.2)_
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
