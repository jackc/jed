# Roadmap / TODO

> Working backlog for the engine. Ordered **roughly** by dependency → importance →
> difficulty, grouped into phases. This is a living file — re-rank freely. The phases
> are a suggested critical path, **not** rigid gates; items marked _(parallel)_ can
> proceed independently.
>
> **The live backlog is every open `- [ ]` line.** `grep '\- \[ \]' TODO.md` is the
> fastest way to find real work. Completed items are collapsed to a **one-line `[x]`
> entry + a pointer to the spec doc** that records the detail — the full design, the
> *why*, the error codes, the golden-fixture names, and the divergence ledgers live in
> `spec/design/*` and git history, **not here**. Open follow-ups hoisted out of done
> items are marked _follow-on:_ and stay in the backlog; size tags `_(size: …)_` are
> kept on open items only.
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

- [x] **Name the project** — settled on **`jed`** (was `abide`); swept code, docs, on-disk magic
      (`ABDB`→`JEDB`), extension (`.adb`→`.jed`), devcontainer ids.

---

## Phase 1 — Foundations: spec backfill + the expression substrate

- [x] **Backfill the EBNF grammar** — the shared contract the parsers conform to. → [grammar.ebnf](spec/grammar/grammar.ebnf), [grammar.md](spec/design/grammar.md)
- [x] **Author the function / operator catalog** — result types + NULL behavior as data, family schema + coherence checker. → [catalog.toml](spec/functions/catalog.toml), [functions.md](spec/design/functions.md)
- [x] **Codegen "middle path"** — catalog → per-language operator descriptor tables, drift-gated by `rake verify`; **errors** also codegen'd (`SqlState` + `ERRORS` from [registry.toml](spec/errors/registry.toml), `gen_errors.rb`). → [gen_catalog.rb](scripts/gen_catalog.rb), [codegen.md](spec/design/codegen.md)
  - [ ] _follow-on:_ extend the generator to **types** — scalars (ragged fields + enum identity threaded through the codec) remain.
- [x] **Resolve integer-literal typing** — context-adaptive untyped constants (adapt to the column/CAST target, trap `22003`, default i64). → [types.md §6](spec/design/types.md)
- [x] **General expression evaluator** — unified recursive `Expr`, one-function-per-precedence-level parser, shared by WHERE + SELECT.
- [x] **Integer arithmetic `+ - * / %` + unary `-`** — trap-on-overflow `22003`, `/`/`%`-by-zero `22012`, promotion-tower result types.
- [x] **`boolean` scalar (expression-only)** — `TRUE`/`FALSE`, comparison/logical results, render tag `B`.
- [x] **Logical connectives `AND`/`OR`/`NOT`** — three-valued (Kleene).
- [x] **`IS [NOT] DISTINCT FROM`** — NULL-safe equality. → [functions.md §3](spec/design/functions.md)
- [x] **Cost-accounting seam** — a deterministic `Meter` through executor/evaluator/storage, data-defined unit schedule, `# cost:` directive, ceiling+abort (`54P01`), `page_read` unit; per-operator `cost` weights codegen'd; `varlen_compare` unit. → [cost.md](spec/design/cost.md), [schedule.toml](spec/cost/schedule.toml) _(§13)_

---

## Phase 2 — Make it feel like SQL (core query/DML completeness)

- [x] **Select-list expressions + `*` + column aliases (`AS`)** — output naming as a cross-core contract (`# names:`). → [grammar.md §8](spec/design/grammar.md)
- [x] **`LIMIT` / `OFFSET`** — either order, non-negative literal (negative → `2201W`/`2201X`). → [grammar.md §9](spec/design/grammar.md)
- [x] **Richer `ORDER BY`** — multiple keys, ASC/DESC, NULLS FIRST|LAST; output-column ordinal (`42P10`); general-expression keys; output-alias keys; general-expression `WITHIN GROUP`; correlated keys; window/`GROUPING()` keys; expression key in a grouped+window query. _(Two deliberate `0A000` non-features: a general-expression key in a set-operation `ORDER BY` — PG also rejects — and GROUPING SETS + window functions.)_ → [grammar.md §10](spec/design/grammar.md)
- [x] **`ORDER BY` satisfied by index/scan order** — PK-prefix scan elides the sort + short-circuits top-N; DESC reverse scan; secondary-index order; streaming `DISTINCT`; two-table INNER/CROSS join on the outer PK prefix. _(Deferred sub-follow-ons: DESC reverse outer; >2 relations; outer non-PK bound; LEFT/RIGHT/FULL; DISTINCT join; index-order join.)_ → [cost.md §3](spec/design/cost.md), [grammar.md §10](spec/design/grammar.md)
- [x] **`DISTINCT`** — NULL-safe dedup, after ORDER BY before LIMIT; PG ORDER-BY-key restriction (`42P10`). → [grammar.md §11](spec/design/grammar.md)
- [x] **FROM-less `SELECT`** — `SELECT 1` over one virtual zero-column row. → [grammar.md §34](spec/design/grammar.md)
- [x] **Predicate forms — `IN`/`BETWEEN`/`LIKE`/`CASE`** — plus `ILIKE`, and the regex operators `~`/`~*`/`!~`/`!~*` + `regexp_replace`/`regexp_match` (a hand-written linear-time Pike VM, ReDoS-immune). → grammar.md §20–§23, [regex.md](spec/design/regex.md)
  - [ ] _follow-on:_ LIKE `ESCAPE 'c'`; `SIMILAR TO` (deliberately excluded — the SQL-standard surface); set-returning `regexp_matches` / `regexp_split_to_table`; the Oracle-compat `regexp_count`/`instr`/`substr`/`like`; Unicode-property char classes (`\p{…}`); backreferences + lookaround (permanently out — they break the linear-time guarantee).
- [x] **Aggregates `COUNT`/`SUM`/`MIN`/`MAX`/`AVG` + `GROUP BY` + `HAVING`** — plus aggregate `DISTINCT`, `FILTER (WHERE …)`, `GROUPING SETS`/`ROLLUP`/`CUBE` + `GROUPING()`, ordered-set aggregates (`mode`/`percentile_cont`/`percentile_disc`), `SELECT DISTINCT` in an aggregate query, `GROUP BY` ordinal/alias/expression, functional-dependency grouping, `FILTER` on a window aggregate, GROUPING SETS + window functions, non-constant/interval/array/collated ordered-set fractions, hypothetical-set aggregates (`rank`/`dense_rank`/`percent_rank`/`cume_dist`). → [aggregates.md](spec/design/aggregates.md)
- [x] **Window functions (`OVER`)** — COMPLETE (S0–S10, all cores) + the sliding/sharing optimization: ranking/offset/aggregate functions, ROWS/RANGE/GROUPS frames + value offsets + EXCLUDE, `WINDOW` named clause + base extension, general-expression keys. Deferred: prefix-compatible sort sharing, invertible moving slide, RANGE offsets over a ts/date key (D4), `FILTER`/`WITHIN GROUP`, `IGNORE NULLS`, correlated keys (`0A000`). → [window.md](spec/design/window.md)
- [x] **Scalar functions `abs` / `round`** — first named per-row functions. → [functions.md §9](spec/design/functions.md)
  - [ ] _follow-on:_ a general implicit argument-coercion pass. (`ceil`/`floor`/`mod`/`sign` and text `length`/`lower`/`upper` have since landed in their own slices.)
- [x] **Scalar string / text functions** — PG's string surface as built-ins with code-point semantics (`length`/`substr`/`lpad`/`btrim`/`replace`/`translate`/`repeat`/`strpos`/`split_part`/`encode`/`decode`/`quote_*`/…). → [string-functions.md](spec/design/string-functions.md)
  - [ ] _follow-on:_ full-Unicode `initcap` word classification + non-ASCII titlecasing; keyword-aware `quote_ident`; a `text::bytea` cast + `length`/`octet_length`/`bit_length` `bytea` overloads; per-character cost metering for `lpad`/`rpad`/`repeat` (the §13 cost-ceiling path; the `54000` hard cap is the current backstop).
- [x] **Named + optional (DEFAULT) function arguments** — PG named notation `f(name => value)` + DEFAULT params; `make_interval`, then `make_timestamp`/`make_timestamptz` (BC year, field overflow `22008`, arity-overloaded session-zone vs explicit-timezone form, `22023`); `VARIADIC` (landed as **AF6** with the array type). → [functions.md §11](spec/design/functions.md)
  - [ ] _follow-on:_ general non-integer DEFAULT values (no consumer yet — built-ins use overloads or `make_interval`-style 0-defaults); user-defined-function defaults (jed has no UDFs).
- [x] **Multi-row `INSERT`** (`VALUES (..),(..)`) — two-phase / all-or-nothing. → [grammar.md §12](spec/design/grammar.md)
- [x] **`INSERT ... SELECT`** — query rows through the same two-phase validation; arity `42601` / type `42804` up front. → [grammar.md §24](spec/design/grammar.md)
- [x] **`DROP TABLE`** (+ `IF EXISTS`, + multi-table `a, b` with `CASCADE`/`RESTRICT`) — two-phase / all-or-nothing; missing `42P01` (no-op under `IF EXISTS`), non-table `42809`, FK-referenced under `RESTRICT` `2BP01`, `CASCADE` drops dependent FKs. → [grammar.md §13](spec/design/grammar.md)

---

## Phase 3 — The type system as the product (the differentiator, §4)

> `boolean`, `text` (collation `C` → linguistic UCA), `decimal`, `timestamp`/`timestamptz`,
> `date`, `interval`, `bytea`, `uuid`, `f32`/`f64`, **`json`/`jsonb`**, and the `array`,
> `range`, and composite containers are all done. What remains: per-type cast/function
> follow-ons, the JSON `0A000` follow-ons, and the composite-container narrowings.

- [x] **Storable `boolean` column** — type code 5, `bool-byte` codec, ORDER BY (false < true), boolean PK/index, `boolean⇄i32` casts. → [types.md §9](spec/design/types.md)
- [x] **`text` + collation** — UTF-8 code-point order (type code 4), text PK/index/UNIQUE via `text-terminated-escape`; **linguistic collation** landed end-to-end: jed-owned UCA executor, `COLLATE` / per-column / per-db default, collated keys, the reference-only/vendored-tier pivot (`format_version 18`, real Unicode-17 root + `es`), and the host-loaded `JUCD` Unicode-data bundle (`db.LoadUnicodeData`). → [types.md §11](spec/design/types.md), [collation.md](spec/design/collation.md), [encoding.md §2.4](spec/design/encoding.md)
  - [ ] _follow-on:_ `varchar(n)` length limits (`22001`); text `||`, `substring`. _(Runtime non-literal text→T casts + `length`/`lower`/`upper` have landed.)_
  - [ ] _follow-on:_ further locale/feature expansion (curated tailorings, nondeterministic collations, `LIKE` under non-`C`, CLDR `shifted`, CJK tier-3 data) — **possibilities, not scheduled work** ([collation.md §14](spec/design/collation.md)).
- [x] **Exact `decimal`** — *the* headline type: sign+coefficient+scale, round-half-away (settles §8), PG result scales, finite-only (documented divergence), decimal PK/index/UNIQUE via `decimal-order-preserving`; `round`/`ceil`/`ceiling`/`floor`/`trunc(x[,n])`, `gcd`/`lcm`/`width_bucket`, and the exact-numeric transcendentals `sqrt`/`ln`/`exp`/`log`/`log10`/`log(b,x)`/`power`/`pow` (hand-rolled `numeric.c` port, byte-identical, no libm on the value path). → [decimal.md](spec/design/decimal.md), [encoding.md §2.5](spec/design/encoding.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods; mixed integer/decimal transcendental arguments (`power(2.0, 3)` needs an explicit cast today); per-work cost metering for the transcendentals (one `operator_eval` per call today).
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, `±infinity` first-class, timestamp PK; the host-loaded `JTZ` tz database + `AT TIME ZONE`; `date_trunc`/`EXTRACT`/cross-family casts in a zone + an observable session `TimeZone` slot. → [timestamp.md](spec/design/timestamp.md), [timezones.md](spec/design/timezones.md)
  - [ ] _follow-on:_ `date_part` (float8), `to_char`/`to_timestamp`, `age`, `EXTRACT(julian …)`; a separate `time` type; **text⇄datetime casts** + **session-zone rendering** of `timestamptz`; `timestamp(p)` precision typmods ([timezones.md §9](spec/design/timezones.md)).
- [x] **`date`** — calendar date (i32 days), strict ISO literals + BC + `±infinity`, date PK (type code 16); date arithmetic (`date ± int`, `date − date`, `date ± interval`). A strict island — no implicit compare to timestamp. → [date.md](spec/design/date.md)
  - [ ] _follow-on:_ runtime text→date cast; clock-relative literals (`today`/`tomorrow`/`now`/`epoch`); remaining date functions (`make_date`, `date_part`, `current_date`). → [date.md §6](spec/design/date.md)
- [x] **Typed string literals + casts (`type 'string'`, `CAST`, `::`)** — generalized typed-literal production, runtime text→T casts on non-literal expressions for the numeric + boolean scalars (`cast.runtime_text`), and the `::` cast operator. → [grammar.md §36/§37](spec/design/grammar.md), [casts.toml](spec/types/casts.toml)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, type code 11, interval PK/index/UNIQUE/FK/GIN via the 16-byte `interval-span-i128` key. → [interval.md](spec/design/interval.md), [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ CAST to/from interval; ISO-8601 `P…` + SQL-standard input; field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `justify_*`/`EXTRACT`/`age`.
- [x] **`bytea`** — variable-width bytes, unsigned order, `\x`-hex literals (`22P02`), type code 7, bytea PK/index/UNIQUE via `bytea-terminated-escape`. → [types.md §13](spec/design/types.md), [encoding.md §2.6](spec/design/encoding.md)
  - [ ] _follow-on:_ traditional escape input (`\nnn`); bytea⇄other casts; binary functions (`length`, `||`, `substring`, `encode`/`decode`, `get_byte`).
- [x] **`uuid`** — fixed 16 bytes, PG-flexible input, canonical output, type code 8, `uuid-raw16` PK; `text ⇄ uuid` / `uuid ⇄ bytea` casts; `uuid_extract_version`/`uuid_extract_timestamp`; `uuidv4()`/`uuidv7([shift])` generators on the host entropy+clock seam. → [types.md §14](spec/design/types.md), [entropy.md](spec/design/entropy.md)
- [x] **Current-time functions** — `now()` (STABLE) / `current_timestamp` / `clock_timestamp()` (VOLATILE) on the clock seam. → [functions.md §12](spec/design/functions.md)
- [x] **`f32` + `f64` (IEEE 754)** — two-width promotion tower, the first types narrowly exempted from byte-identity (the `R` tolerant compare + exception ledger), type code 12; **float in a PK/index** (`float-order-preserving` key, every scalar now keyable, only `composite` stays `0A000`); the float math functions (`cbrt`/`pi`/`radians`/`degrees`, inverse + hyperbolic trig, the exact numerics `sign`/`mod`/`div`/`gcd`/`lcm`/`factorial`/`width_bucket`/`scale`/`min_scale`/`trim_scale`). → [float.md](spec/design/float.md), [determinism.md](spec/design/determinism.md)
  - [ ] _deferred:_ the `width_bucket(value, thresholds[])` array-threshold variant.
- [x] **`json` / `jsonb` + SQL/JSON** — the committed XL headline feature: all non-deferred slices across all three cores, oracle-clean; spec'd across [json.md](spec/design/json.md), [jsonpath.md](spec/design/jsonpath.md), [json-sql-functions.md](spec/design/json-sql-functions.md), [json-table.md](spec/design/json-table.md); type codes 18/19/20, one `format_version` bump (v18→v19).
  - [ ] _follow-ons (deferred `0A000`, hoisted from the done slices):_ the string-**dictionary builder** (opens the [json.md §3](spec/design/json.md) door); `jsonb`-as-PK/index ([encoding.md §2.13](spec/design/encoding.md)); GIN **`jsonb_ops`** opclass for `@>`/`?`; `JSON_TABLE` explicit `PLAN` (T2); `ON ERROR/EMPTY DEFAULT <expr>` (S3); the remaining **jsonpath** surface (`like_regex` → Pike VM, item methods `.type()`/`.size()`/`.double()`/…, arithmetic, `vars`/`silent` args, the `_tz` query-function variants — P2/P3); the **verbatim-`json`** SRF / accessor variants (`json_array_elements[_text]`, `json_each[_text]`, the `->`/`#>` json overloads); `jsonb_set_lax`; `row_to_json`; in-aggregate `ORDER BY` for `json[b]_agg`.
- [x] **`array` type** — the second container axis: structural `Type::Array(Box<Type>)`, shape-as-value (PG-faithful), null-bitmap codec, btree-NULL element comparison. **COMPLETE** — S0–S5 (`format_version` 10; subscripting; multidim values + custom lower bounds + slices), the AF1–AF7 function/operator surface (`anyarray`/`anyelement` polymorphism, `||`/`@>`/`<@`/`&&`, `unnest`, `ANY`/`ALL`/`SOME`, `VARIADIC`), composite-element arrays, the three runtime casts (`cast.array`), and arrays-in-keys (`array-elements-terminated`, the second container key; a `float`-element array key landed with the float-key slice). **Every array follow-on has landed.** → [array.md](spec/design/array.md), [array-functions.md](spec/design/array-functions.md)
- [x] **`range` type** — the third container axis: PG's six built-in range types (`i32range`/`i64range`/`numrange`/`tsrange`/`tstzrange`/`daterange`), structural over a bounded subtype set, total range btree order, `format_version` 16 / type code 17; R0–R3 + the RF1–RF4 accessor/constructor/boolean/set-op surface + range-as-key (`range-bounds`, the first container key). Deferred: range index/GiST, multirange, `range_agg`. → [ranges.md](spec/design/ranges.md)
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — COMPLETE (S0–S6): the open `Type { Scalar | Composite(catalog-ref) }`, `CREATE`/`DROP TYPE`, nested + recursive types, storable composite column + recursive codec (`format_version` 9), `ROW(…)`, field access, element-wise compare/ORDER BY/DISTINCT/GROUP BY. Named composites only. → [composite.md](spec/design/composite.md)
  - [ ] _still narrowed (relaxable later):_ `INSERT … SELECT` / `UPDATE` of a composite column; composite `PRIMARY KEY`/index/`UNIQUE` (`0A000` — key encoding authored, unexercised); `DEFAULT` on a composite column; runtime non-literal text→composite + `composite::text` + anonymous `ROW(…)::type` casts; the nested `ROW(ROW(…),…)`-into-column constructor.

---

## Phase 4 — Relational depth + constraints

- [x] **`JOIN` — multi-table FROM + INNER/CROSS + outer (LEFT/RIGHT/FULL)** — left-deep nested-loop executor, aliases, qualified refs; plus comma-`FROM`, `t.*`, `USING`, `NATURAL`. Deferred: `FULL JOIN ... USING` / `NATURAL FULL JOIN` (the COALESCE merge). → [grammar.md §15](spec/design/grammar.md)
- [x] **Subqueries** — uncorrelated scalar, `[NOT] IN (SELECT …)`, `[NOT] EXISTS`, correlated, subqueries in UPDATE/DELETE, `$N` inside a subquery, derived tables, a `VALUES` body, `LATERAL`, `x op ANY/ALL(SELECT …)`. → [grammar.md §26/§42/§44](spec/design/grammar.md)
  - [ ] _follow-on:_ a correlated `GROUP BY` / `ORDER BY` key (`0A000`, degenerate).
  - [ ] _follow-on:_ a **parenthesized-join FROM** (`FROM (a JOIN b ON …)`); a trailing **`ORDER BY`/`LIMIT` on a VALUES body**.
  - [ ] **Subqueries — remaining seams:** subqueries in an **`INSERT ... VALUES`** slot (blocked on VALUES holding a general expression); **row-valued** subqueries. _(size: S)_
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — precedence tree (INTERSECT binds tighter), full per-column type unification, NULL-safe multiset semantics, trailing ORDER BY by name/ordinal. → [grammar.md §25](spec/design/grammar.md)
  - [ ] _follow-on:_ parenthesized operands `(SELECT …) UNION …`; ORDER BY/LIMIT inside an operand; ORDER BY ordinals; a set op in an `INSERT … SELECT` source.
- [x] **Common table expressions (`WITH`)** — named derived tables (PG hybrid inline/materialize), `WITH RECURSIVE`, data-modifying (writable) CTEs, nested `WITH`. → [cte.md](spec/design/cte.md), [recursive-cte.md](spec/design/recursive-cte.md), [writable-cte.md](spec/design/writable-cte.md)
  - [ ] _follow-on:_ a nested `WITH` **inheriting enclosing CTEs** (the residual visibility divergence); recursive-CTE deferrals (`SEARCH`/`CYCLE`, a set-op / `FROM`-subquery recursive term, mutual recursion).
- [x] **Set-returning functions** — `generate_series` in FROM, a synthetic one-column relation, a `generated_row` cost unit. → [functions.md §10](spec/design/functions.md)
  - [ ] _follow-on:_ the column-alias-list `AS g(c)`. (`LATERAL` ✅ landed; `unnest(array)` ✅ landed — AF3.)
- [x] **`NOT NULL`** — storing NULL → `23502`. → [constraints.md §1](spec/design/constraints.md)
- [x] **`DEFAULT` (literal + expression)** — literal coerced once at CREATE TABLE; non-constant `DEFAULT <expr>` (`uuidv7()`, `1 + 1`) stored as text + evaluated per row through the entropy/clock seam (`format_version` 8). → [constraints.md §2](spec/design/constraints.md)
  - [ ] _follow-on:_ `UPDATE ... SET x = DEFAULT` and `INSERT ... DEFAULT VALUES`.
- [x] **Composite `PRIMARY KEY`** — table-level `PRIMARY KEY (a, b, …)`, key bytes = members' concatenated encodings. → [constraints.md §3](spec/design/constraints.md)
  - [ ] _follow-on:_ composite point-lookup / prefix pushdown (a composite-PK table full-scans today — an optimization slice with its NoREC obligation).
- [x] **`CHECK` constraints** — column-/table-level, enforced per candidate row in the two-phase pass (`23514`), `format_version` 4. → [constraints.md §4](spec/design/constraints.md)
- [x] **`UNIQUE` constraints + unique indexes** — `UNIQUE` and `CREATE UNIQUE INDEX`; a UNIQUE constraint **is** its backing unique index; NULLS-distinct; `format_version` 6. → [constraints.md §5](spec/design/constraints.md)
- [x] **`FOREIGN KEY` constraints** — column-/table-level `REFERENCES`, composite + self-reference, same-type pairing (`42804`), MATCH SIMPLE, enforced at four write sites (`23503`), `format_version` 11. → [constraints.md §6](spec/design/constraints.md)
  - [ ] _follow-on:_ the referential **actions** `ON DELETE/UPDATE CASCADE | SET NULL | SET DEFAULT` (parse but `0A000` today); `MATCH FULL`; a **backing index** on the child FK columns (the parent-side check full-scans children today); FK type pairing relaxed to PG's comparable-types; `ALTER TABLE … ADD/DROP CONSTRAINT`.
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — non-unique on-disk B-trees, maintained in the two-phase pass; the planner index-bounds a base scan on a first-column equality; `format_version` 5. → [indexes.md](spec/design/indexes.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ index ranges / multi-column prefixes; index scans for UPDATE/DELETE (keep PK pushdown today); LIMIT-streaming combination; expression/ordered/partial keys; `IF NOT EXISTS`. (All scalar key types are now encodable; only the recursive `composite` container stays a `0A000` key.)
- [x] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* via a type-generic operator-class seam: the **`array_ops`** opclass (one entry per distinct non-NULL element, `format_version` 12's `index_kind` byte, a `gin_entry` cost unit) accelerating `@>`/`&&`/`= ANY(col)`/array `=` for SELECT + GIN-bounded UPDATE/DELETE, over the fixed-width key-encodable element types. → [gin.md](spec/design/gin.md)
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows) / `IN` over a scalar list; the **remaining** element types — the VARIABLE-width keyables (`text[]`, `bytea[]`, `decimal[]`) need GIN term framing; `float[]` and `interval[]` are now UNBLOCKED (fixed-width keys landed) but each is its own slice — plus composite-element arrays; multi-column GIN; correlated / array-column query operands; the ordered-index equality bound for UPDATE/DELETE; the LIMIT-streaming combination; posting-list run compression; the **`jsonb_ops`** opclass and a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints** — a third index *kind* (`index_kind = 2`) whose payoff is PG exclusion constraints (`EXCLUDE USING gist (col WITH op)`, `23P01`); an operation-deterministic R-tree (a structural divergence — jed's own tree bytes), the `range_ops` + fixed-width scalar-`=` opclasses, multi-column GiST; `format_version` 21. → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ the `EXCLUDE … WHERE (predicate)` partial form; `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique index; `ALTER TABLE … ADD CONSTRAINT … EXCLUDE`; **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape.
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <items>` evaluated after validation before any write; the PG-18 `old.`/`new.` row-version qualifiers landed. → [grammar.md §32](spec/design/grammar.md)
  - [ ] _follow-on:_ the `WITH (OLD AS o, NEW AS n)` aliasing form; `old.*`/`new.*`.
- [x] **Sequences** (`CREATE SEQUENCE` / `nextval` / `currval`) — landed S0–S6: a persisted monotonic i64 generator (`entry_kind = 2`, `format_version` 12), `nextval`/`currval`/`setval`/`lastval`, `serial`/IDENTITY owned-sequence columns (`format_version` 14/15), `ALTER SEQUENCE`. The defining decision — `nextval` is **transactional** (a deliberate PG divergence, determinism.md §5). → [sequences.md](spec/design/sequences.md)
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] }`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter; two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT` (with the `UPDATE` `SET = DEFAULT` follow-on); `INSERT INTO t AS alias`; the partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed, but the conflict-path re-key is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [x] **Relax the UPDATE narrowings** — assigning a `PRIMARY KEY` column now **re-keys** the row (it moves, secondary-index entries follow); end-state validation traps `23505`/`23503`, no `format_version` bump. The DO UPDATE conflict-path equivalent remains deferred `0A000`. (§11 step 6.)
- [ ] **Temporary tables** — `CREATE [SHARED] [TEMP|TEMPORARY] TABLE` (+ `DROP`): relations that make **zero writes to the database file** (held outside the serialized `Snapshot`, no `format_version` bump), bounded by a deterministic storage budget to keep the untrusted-SQL guarantee (§13). Namespace precludes overlaps (`42P07`); new code `54P03 temp_storage_limit_exceeded`; `allow_ddl` splits into `allow_ddl` / `allow_temp_ddl` / `allow_shared_temp_ddl`. **Landed:** slices 1–2 (session-local memory-only + database-wide shared with the two-root commit), CREATE/DROP INDEX on a temp table, serial/IDENTITY, composite-typed columns. **Open:** **slice 3 — spill-to-disk** (the resident→paged flip onto a temp `BlockStore`; the seam is already open). → [temp-tables.md](spec/design/temp-tables.md) _(size: L; deps: session model (done), storage seam (done))_
  - [ ] _follow-on:_ `ON COMMIT DELETE ROWS`/`DROP`; `IF NOT EXISTS`; `CREATE TEMP TABLE … AS SELECT`; FKs among same-kind temp tables; temporary views. → [temp-tables.md §14](spec/design/temp-tables.md)

---

## Phase 5 — Transactions & the §3 commit model

> ✅ **Phase 5 is landed (P5.0–P5.4, all three cores).** Immutable **`Snapshot`**s + a writer's
> **working root** over a persistent (copy-on-write) ordered B-tree; PostgreSQL autocommit;
> commit decoupled from durability via a `synchronous` setting; the oldest-live-txid
> **watermark** as the Phase-6 free-list gate. → [transactions.md](spec/design/transactions.md)

- [x] **P5.0–P5.3** — the transaction model spec + class-25 errors; the persistent ordered map (`pmap` CoW B-tree) + snapshot refactor; explicit `BEGIN`/`COMMIT`/`ROLLBACK` (+ `READ ONLY|WRITE`) and the `Transaction` API; reader/writer concurrency + the `SharedDb` watermark. → [grammar.md §27](spec/design/grammar.md), [api.md §2.5/§6](spec/design/api.md)
- [x] **P5.4 — cross-core concurrency conformance** — the `# format: concurrency` schedule (Layer 1), the write-gate `blocks` annotation (Layer 2), the `stress/*.stress.toml` parallelism-stress format (Layer 3, `rake stress`, outside `rake ci`). → [concurrency-testing.md](spec/design/concurrency-testing.md)

---

## Phase 6 — Storage maturation (§9)

> Can lag the feature work until write volume makes whole-image rewrites costly. These items
> are also the path to a **larger-than-RAM file that does not fall over** (CLAUDE.md §9): no
> full-residency assumption above the storage seam.

- [x] **P6.1–P6.4** — incremental COW commit = page-backed B-tree (`format_version` 2, meta-slot root swap); free-list / page reclamation (reconstruct-on-open); the logical `page_read` cost unit; the buffer pool / demand paging (bounded leaf cache, CLOCK eviction, `cache_pages` budget). → [storage.md §4/§6](spec/design/storage.md), [pager.md](spec/design/pager.md)
  - [ ] _follow-on (where the watermark does real work):_ continuous *within-session* reclamation (return a commit's orphans immediately, paired with file-backed reader sharing); on-disk free-list persistence (claim meta offset 28 to skip the open-time reachable-set walk).
- [ ] **File compaction / shrink (return space to the OS)** — ⏳ **approach decided, not built.** The free-list recycles dead space for jed but `page_count` is a monotonic high-water, so the file is grow-only. Decided mechanism: a **host-invoked compaction** that re-serializes the committed snapshot through the from-scratch `to_image` serializer into a fresh file + atomic swap (the `create` temp-file + fsync + rename recipe), reclaiming all dead space + defragmenting (the SQLite `VACUUM` / PG `VACUUM FULL` flavor) crash-safely. Explicit / host-invoked, gated on the reader-liveness watermark; needs nothing new at the storage seam. A lighter in-place trailing-free truncation stays open as a cheaper partial complement. → [storage.md §6](spec/design/storage.md) _(size: M–L; deps: P6.2; §9)_
- [ ] **Streaming + spill-to-disk operators** — bound blocking operators (`ORDER BY`, hash `JOIN`, `GROUP BY`/aggregate, `DISTINCT`) by a memory budget and **spill to disk** when exceeded, so a query over larger-than-RAM data never materializes its whole input/output in memory. **Landed:** the **external merge sort for `ORDER BY`** (a `Sorter` bounded by `work_mem`, spills sorted runs + k-way merges, byte-for-byte identical to the in-memory sort). → [spill.md](spec/design/spill.md) _(size: XL; deps: paged storage; §9/§13)_
  - [ ] **Spilling hash aggregate / `DISTINCT` / hash JOIN** — the remaining blocking operators (spill.md §7). Each needs a *different* algorithm: a partitioned (grace) hash that preserves first-occurrence order for aggregate/DISTINCT, and — for hash JOIN — a hash-join operator first (jed joins are nested-loop today), then grace-hash spill to bound the build side. _(size: L–XL each)_
- [ ] **Bench-driven perf follow-ons** — the measured gaps remaining after the `perf-point-lookup` work (which took `point_lookup_pk` past same-language PG clients in all 3 cores):
  - [ ] **Rust CoW insert deep-clone** — `node_insert` rebuilds a path node with `Vec::clone`, deep-copying every key (`Vec<Vec<u8>>`) + row where Go's `[][]byte` copy is pointer-shallow (why `insert_rollback` is rust 21.6ms vs go 10.3ms). Fix: share entry storage (`Arc<[u8]>` keys / `Arc`-shared rows). Rust-only, no byte or cost change. _(size: M)_
  - [ ] **ORDER BY + LIMIT top-k** — `order_by_limit` full-sorts all 1M rows before slicing (0.76–1.6s vs PG ~20ms). A bounded top-k selection (heap of LIMIT+OFFSET, index-stable tie-break) cuts the sort to ~scan cost. Rows + cost unchanged. _(size: M; ×3 cores)_
  - [ ] **Full-scan materialization** — `full_scan_agg` clones every row into a buffer before aggregating (143–281ms vs PG ~13ms). Streaming aggregation over the scan visitor is the contained first step; the full fix is the spill item above. _(size: M–L)_
- [x] **Large values — overflow pages + compression (TOAST-equivalent)** — large `text`/`bytea`/`decimal`/`json` pushed out-of-line onto overflow-page chains (`format_version` 3), optionally LZ4-compressed first via a deterministic hand-rolled block codec (no third-party dep — a library fails §8 byte-identity). → [large-values.md](spec/design/large-values.md), [lz4.md](spec/fileformat/lz4.md)
  - [ ] _follow-on:_ chain sharing on rewrite (let a rewritten record keep an unchanged value's existing chain — a byte-layout change, lands in all cores + incremental tests together).
- [x] **Crash-recovery hardening** — a pager fault-injection seam + a cross-core recovery matrix proving a crash *anywhere* recovers to a valid snapshot; WAL stays deferred (COW + root-swap gives atomicity without one). → [storage.md §7](spec/design/storage.md)

---

## Phase 7 — Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters + sessions
> have landed; OPFS spill + the e2e-in-CI gap remain. Parallelizable with most feature work.

- [x] **Formal public API** — `create`/`open`, crash-safe explicit `commit`/`close`, `prepare`, execute, a `Rows` cursor, structured errors; affected-row counts in `Outcome`; same shape across all three cores. → [api.md](spec/design/api.md)
- [x] **Parameterized queries (`$1`)** — lexed/parsed, context-typed at resolve (`42P18` if indeterminate), bound two-phase before any scan; tested per-core.
- [x] **Cost ceiling (`max_cost`) + deterministic abort** — a handle `max_cost` aborts with `54P01` the instant accrued cost reaches it; plus a fixed `MAX_EXPR_DEPTH = 256` parser nesting bound (`54001`). → [cost.md §6/§7](spec/design/cost.md)
- [x] **The `jed` CLI** — a full-screen TUI client (Rust + ratatui) + a script mode (`-c`/`-f`/stdin; aligned/csv/json), with autocomplete + highlighting, CSV import/export, `--dump`, `-o`, `box`/`markdown`, `--readonly`. A host program, not a core. → [cli.md](spec/design/cli.md)
- [x] **Sessions — the configured host context** — un-fused `Database` from a first-class `Session`: the stateful default session + explicit tx state machine (S1), the `split_statements`/`execute_script` iterator (S2), the GRANT/REVOKE privilege envelope + `allow_ddl` (`42501`, S3), the `lifetime_max_cost` cumulative budget (`54P02`, S4), session variables + `current_setting()` (S5), and the built-in `time_zone` slot (default `UTC`, S6). → [session.md](spec/design/session.md)
- [ ] **Storage hosts** — the five-method `BlockStore` byte device, host catalog, and decoration layering (encryption codec above the seam, replication tee below) authored in [hosts.md](spec/design/hosts.md). **Landed:** the Node `fs` host, the `FileBlockStore` extraction, and the **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity vs goldens, gated Playwright e2e); Rust/Go inline `std::fs`/`os` in the per-core `Pager`. **Open:** OPFS disk-spill, the e2e in CI. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [ ] **(Open question, not scheduled)** low-level direct access API beneath SQL (`getValue("table", key)`) — keep the seam open, don't build yet (§9). _(size: —)_

---

## Phase 8 — Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Several items are **ongoing obligations**
> that grow with each feature, not one-shot tasks.

- [ ] **Differential-testing harness** vs the PostgreSQL oracle (§7) — **PARTIAL.** The live-`db` oracle-import tool is built (`scripts/oracle_import.rb`; `rake corpus:import/check`; the override ledger `spec/conformance/oracle_overrides.toml`). *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated** reference provisioning §12 — never auto-provision). SQLite is deliberately not an oracle; mining its sqllogictest corpus for query *shapes* (answers from PG) is the only oracle-adjacent use. _(size: M remaining)_
- [ ] **SQLancer-style metamorphic / generative testing** — **PARTIAL.** Built so far (`scripts/norec_gen.rb`; `rake corpus:norec_sweep`, in `rake ci`): the **NoREC** slice (pushdown vs non-optimizable rewrite must agree), the **TLP** slice (ternary-logic partitioning), and an automatic **reducer** (`scripts/reduce.rb`, ddmin). *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator), aggregate `GROUP BY` TLP (blocked on `COALESCE`/`LEAST`/`GREATEST`), and broader NoREC relations. _(size: M remaining)_
- [x] **Result-type assertion directive** — `# types:` asserts each result column's precise resolved type (`i16` vs `i32`); `numeric(p,s)` typmod granularity stays deferred. → [conformance.md §7](spec/design/conformance.md)
- [ ] **Corpus growth** (ongoing) — keep adding `.test` coverage as each feature lands. Two **standing obligations** (conformance.md §5/§8): (a) on the PG-comparable surface, run `rake corpus:check` and register any intentional divergence in the override ledger; (b) **when you add a query optimization or a new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does not discover new optimizations. (Future index/DISTINCT/aggregate pushdown are not yet covered.)
- [ ] **Benchmark backfill** (ongoing) — grow `bench/corpus` beyond the v1 set (benchmarks.md §11): a join benchmark (needs a second dataset table → `generator_version` bump), GROUP BY aggregate, UPDATE/DELETE throughput, miss-heavy point lookups, text/large-value-heavy rows, `SharedDb` concurrent-reader throughput, cold-open time, durable-commit batch-size sweep. **Standing obligation** (§10): a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected benchmarks before/after. _(size: M, ongoing)_

---

## Phase 9 — Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, [cores.md](spec/design/cores.md)).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). Any native core still passes the full conformance contract; a wrap inherits it.

- [x] **Ruby gem** — wrap the safe Rust core, shipped as a gem (conforms by construction; a distribution artifact, not an independent conformance voice). **Landed:** Slice 1 (the `cdylib` + a `fiddle`-loaded pure-Ruby gem), Slice 2 (`$N` bind params), Slice 3 (richer typed values — `BigDecimal`/`Date`/`Time`, AR-style), Slice 4 (host-loaded `JUCD`/`JTZ` bundles), and the binding-overhead benchmark (`bench/ruby`). → [ruby.md](spec/design/ruby.md), [cores.md §6](spec/design/cores.md)
  - [ ] _follow-on (each its own slice):_ a **gem prepared-statement API** (isolates the pure FFI tax from per-call parse); **`interval`/`uuid`/`bytea`** typed coercion (left as String); **distributable packaging** — a `gem install`-able native gem via `rb-sys` + precompiled platform gems (or `magnus`), replacing the in-repo `rake ruby:build` (a wrapper-module dep — needs §14 confirmation); an optional **Ruby conformance runner**. In-process Ruby host functions ride on the vectorized/batched host-function API below. _(size: L wrap)_
- [ ] **C#** core — **lean native** (value-type generics, `Span<T>`, NativeAOT → near-Rust speed *and* a clean pure-managed NuGet package; in-process host functions). Strongest native candidate. _(size: XL native / L wrap; §2)_
- [ ] **Swift** core — **lean wrap** (UniFFI + XCFramework over the safe Rust core: Rust speed, well-trodden Apple packaging, untrusted-query safety preserved §13). Native only if hot-path per-row host functions make the FFI upcall tax dominant. _(size: L wrap / XL native; §2/§13)_
- [ ] **Java** core — **conflicted**: wrap for performance (pre-Valhalla boxing + JIT warmup hurt a native core), native for clean pure-JAR packaging + in-process host functions (no JNI/upcall tax). Decide at scheduling time; Valhalla shifts it toward native. _(size: XL native / L wrap; §2)_
- [x] **Runtime function registry** — built-in named scalar + aggregate resolution is data-driven over the generated catalog tables (one `(name, arg_families)` lookup). → [extensibility.md §5](spec/design/extensibility.md)
  - [ ] _follow-on:_ built-in type-vtable dogfood (Fork A) and host registration into the table.
- [ ] **Design the host-function API vectorized/batched** up front — the single decision that keeps wrapping viable (amortizes the per-row FFI upcall). Sits on the runtime function registry above — host functions register into the same `(name, arg_families)` table; a host name colliding with a built-in is rejected (propose `42723`). _(size: M; §2, cross-cutting)_
- [ ] **Host-defined functions must contribute to the cost system** — a hard requirement on the host-function API above, not optional. An unmetered host function breaks two contracts: the untrusted-query bound (§13 — an unmetered call burns unbounded CPU past `max_cost`) and the cross-core cost identity (§8). So registration **must** carry a cost-contribution contract. Design space (decide when scheduled, recorded in cost.md §6): a **declared static weight**; a **declared cost-as-a-function-of-arguments** (pure, charged up front and guarded before the call); or a **metering callback** (a narrow deterministic `charge(n)` handle enabling chunk-boundary mid-call abort). A host that declines all three is admissible only on `max_cost = 0` — not the untrusted-query surface. _(size: M; §2/§13)_

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
- **Encryption at rest (file-level).** Whole-file or per-page encryption is a door to keep
  open, **designed in [encryption.md](spec/design/encryption.md)**: a page codec in the core
  above the block seam, a standardized AEAD under a deterministic `(page_index, txid)` nonce
  (keeps §8 byte-identity; the auth tag closes the `format_version` 7 CRC tamper gap), crypto
  from a vetted library (§14). The only present requirement is non-foreclosure — already
  satisfied.
- **Replication.** ✅ **Architecture decided (block-shipping, no WAL), not built** — designed in
  [replication.md](spec/design/replication.md). Ship the **per-commit page-delta** in `txid`
  order, as a tee at the block seam (below the encryption codec → keyless backup replicas). No
  WAL: COW + the root swap already give atomicity *and* lock-free concurrency. Trade:
  write-amplification. A **logical** changeset stream is a separate higher-layer door, not
  foreclosed, not scheduled.
