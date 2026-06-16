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
      column/CAST target, trap `22003` out of range, default int64). → [types.md §6](spec/design/types.md)
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
      function-call syntax, whole-table + grouped aggregation, PG widening (SUM int→int64/decimal,
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
> `timestamp`/`timestamptz`, `interval`, `bytea`, `uuid`, and `float32`/`float64` are done;
> `json` and `array` are the remaining headline items.

- [x] **Storable `boolean` column type** — on-disk type code 5, `bool-byte` codec, comparison +
      ORDER BY (false < true, NULLs last). → [types.md](spec/design/types.md)
  - [ ] **boolean in a key / `PRIMARY KEY`** — rejected `0A000`; the `bool-byte` key rule is
        authored but unexercised. _(size: S)_
  - [ ] **boolean⇄integer casts** — rejected `0A000`/`42804`; PG's are asymmetric, so a dedicated
        cast slice. _(size: S; §5)_
- [x] **`text` + ONE collation (`C`)** — UTF-8 byte/code-point order, on-disk type code 4, first
      operator overload; the UTF-8-vs-UTF-16 ordering trap handled in TS. → [types.md §11](spec/design/types.md)
  - [ ] _follow-on:_ text in a PRIMARY KEY/index (`0A000`, key encoding authored/unexercised);
        `varchar(n)` length limits (`22001`); runtime non-literal text→T casts; string functions
        (`||`, `length`, `lower`/`upper`, `substring`); multi-collation / ICU / `COLLATE`.
- [x] **Exact `decimal`** — *the* headline type: hand-rolled sign+coefficient+scale, round-half-away
      (settles the §8 rounding hotspot), PG result scales, first parameterized + first cross-family
      promotion; finite-only (documented PG divergence). → [decimal.md](spec/design/decimal.md)
  - [ ] _follow-on:_ decimal in a PRIMARY KEY/index (`0A000`); negative / `s>p` scale typmods;
        `round(x,n)` and other decimal functions.
- [x] **`timestamp` / `timestamptz`** — PG instant model, int64 µs, no tz database, `±infinity`
      first-class, timestamp PK supported. → [timestamp.md](spec/design/timestamp.md)
  - [ ] _follow-on:_ `EXTRACT`/`date_trunc`/`age`; separate `date`/`time` types; named-zone
        `AT TIME ZONE`; timestamp⇄text/date casts; `timestamp(p)` precision typmods.
- [x] **Typed string literals + string-literal casts (`type 'string'`)** — one generalized
      production = `CAST('string' AS type)`; literal-only coercion preserves strictness.
      → [grammar.md §36](spec/design/grammar.md)
  - [ ] _follow-on:_ runtime text→T cast on a non-literal text expression (shared with the text follow-on).
- [x] **`::` cast operator** (`expr :: type`) — desugars to the `Cast` node; binds tighter than
      unary minus; a bind-param operand takes the cast target as its type. → [grammar.md §37](spec/design/grammar.md)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, the
      engine's first timestamp arithmetic; on-disk type code 11. → [interval.md](spec/design/interval.md)
  - [ ] _follow-on:_ interval PK/index (`0A000`); CAST to/from interval; ISO-8601 `P…` + SQL-standard
        input; field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `justify_*`/`EXTRACT`/`age`.
- [x] **`bytea`** — variable-width bytes, unsigned byte order, `\x`-hex literals (`22P02` on bad
      hex), on-disk type code 7. → [types.md §13](spec/design/types.md)
  - [ ] _follow-on:_ bytea PK/index (`0A000`); traditional escape input (`\nnn`); bytea⇄other casts;
        binary functions (`length`, `||`, `substring`, `encode`/`decode`, `get_byte`).
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
- [x] **`float32` + `float64` (IEEE 754)** — two-width promotion tower; the first types **narrowly**
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
  - [ ] _follow-ons (each its own slice + obligations):_ array-of-composite elements; arrays-in-keys
        (`0A000`, encoding authored §8); the array function/operator surface
        (`array_length`/`cardinality`/`unnest`/`||`/`@>`/`&&`/… + the polymorphic
        `anyarray`/`anyelement` resolution + `ANY`/`ALL`/`VARIADIC`); runtime text→array,
        `array::text`, and element-wise array→array casts.
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
  - [ ] **Subqueries — remaining seams:** subqueries in an **`INSERT ... VALUES`** slot (blocked on
        VALUES holding a general expression); **derived tables** (`FROM (SELECT …) AS t`); **`ANY` /
        `ALL`** and row-valued subqueries. _(size: M)_
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — a query-expression
      precedence tree (INTERSECT binds tighter), full-PG per-column type unification, NULL-safe
      multiset semantics, trailing ORDER BY by output-column name. → [grammar.md §25](spec/design/grammar.md)
  - [ ] _follow-on:_ parenthesized operands `(SELECT …) UNION …`; ORDER BY/LIMIT inside an operand;
        ORDER BY ordinals; a set op in an `INSERT … SELECT` source.
- [ ] **Common table expressions (`WITH`)** — `WITH name [(cols)] AS (SELECT …) [, …] SELECT …`:
      named subqueries visible as relations in the statement's FROM (and to later CTEs in the
      same WITH list). A CTE is essentially a **named derived table**, so it builds on the
      derived-tables seam (the subqueries follow-on above): the scope machinery must serve
      relations that aren't catalog tables. Decide the evaluation rule as a deterministic cost
      contract (PG ≥12 inlines a single-reference CTE and materializes a multi-reference /
      `MATERIALIZED` one — jed needs one defined rule, recorded in cost.md §3). Follow-on slices,
      not this item: **`WITH RECURSIVE`** (the iterate-to-fixpoint executor + a termination story —
      the `54P01` cost ceiling does real work there) and **data-modifying CTEs**
      (`WITH x AS (INSERT … RETURNING …)`). _(size: L; +L for RECURSIVE; deps: derived tables)_
- [x] **Set-returning functions** — `generate_series(start, stop [, step])` in FROM position, a
      synthetic one-column relation, a new `generated_row` cost unit; integer variants (timestamp
      waits on interval composition). → [functions.md §10](spec/design/functions.md)
  - [ ] _follow-on:_ `LATERAL`, the column-alias-list `AS g(c)`, `unnest(array)` (once arrays land).
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
- [ ] **Constraints (remaining)** — `FOREIGN KEY`. Heavier. _(size: L)_
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — non-unique on-disk B-trees of
      empty-payload records, maintained in the two-phase pass; the planner index-bounds a SELECT
      base scan on a first-column equality; `format_version` 5 catalog reshape; DROP code `42809`.
      → [indexes.md](spec/design/indexes.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ index ranges / multi-column prefixes;
        index scans for UPDATE/DELETE (keep PK pushdown today); LIMIT-streaming combination;
        non-key-encodable index types (text/decimal/bytea/boolean keys); expression/ordered/partial
        keys; `IF NOT EXISTS`.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <select_items>` projecting affected
      rows (INSERT stored / UPDATE new / DELETE old), evaluated after validation before any write;
      the PG-18 `old.`/`new.` row-version qualifiers landed as a follow-on.
      → [grammar.md §32](spec/design/grammar.md)
  - [ ] _follow-on:_ the `WITH (OLD AS o, NEW AS n)` aliasing form; `old.*`/`new.*`.
- [ ] **`UPSERT` / `ON CONFLICT`**. _(size: M; deps: UNIQUE ✅, RETURNING ✅ — unblocked)_
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
- [x] **The `jed` CLI** — a full-screen TUI client (Rust + ratatui/crossterm/tui-textarea, the
      §14-approved deps) + a plain script mode (`-c`/`-f`/stdin; aligned/csv/json). A host program,
      not a core. → [cli.md](spec/design/cli.md)
- [x] **Affected-row counts in `Outcome`** — DML without RETURNING reports rows touched (PG command
      tags), an additive `Outcome` field in all 3 cores. → [api.md §4](spec/design/api.md)
- [x] **CLI follow-ons** — editor autocomplete + syntax highlighting, CSV import/export, `--dump`
      SQL export, `-o` redirection, `box`/`markdown` formats, `--readonly` open mode. → [cli.md §8](spec/design/cli.md)
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
      precise resolved type (`int16` vs `int32`) beyond the render tag; `numeric(p,s)` typmod
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
