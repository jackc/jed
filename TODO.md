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

- [x] **`EXPLAIN` / `EXPLAIN ANALYZE`** — render the planner's chosen plan as a deterministic
  `depth`/`node`/`detail` result set (pre-order, `nosort`), without executing the inner statement;
  `ANALYZE` runs it and reports the actual (deterministic) accrued cost + row count on an `Analyze`
  root. Covers read queries + DML (plan-only, never mutates); `ANALYZE` of a write executes + commits.
  The observability substrate for the cost-based planner. → [explain.md](spec/design/explain.md)
  - [ ] _follow-on:_ estimated-cost columns (`est_rows`/`est_cost`) once a **plan-time cost
    estimator** lands (the reason the structured-column shape was chosen — the doorway to a
    cost-based planner); per-node cost attribution under `ANALYZE`; a full expression printer for the
    residual filter / projections (currently a `conjuncts=N` count) + exact float-literal bound
    rendering (each needs a determinism-ledger entry); an `EXPLAIN (…)` option list; a
    streaming/buffered/deferred lane tag; the DML touched-set count; `EXPLAIN` of a data-modifying `WITH`.
- [x] **Predicate forms — `IN`/`BETWEEN`/`LIKE`/`CASE`** — plus `ILIKE`, and the regex operators `~`/`~*`/`!~`/`!~*` + `regexp_replace`/`regexp_match` (a hand-written linear-time Pike VM, ReDoS-immune). → grammar.md §20–§23, [regex.md](spec/design/regex.md)
  - [ ] _follow-on:_ LIKE `ESCAPE 'c'`; `SIMILAR TO` (deliberately excluded — the SQL-standard surface); set-returning `regexp_matches` / `regexp_split_to_table`; the Oracle-compat `regexp_count`/`instr`/`substr`/`like`; Unicode-property char classes (`\p{…}`); backreferences + lookaround (permanently out — they break the linear-time guarantee).
- [x] **Scalar functions `abs` / `round`** — first named per-row functions. → [functions.md §9](spec/design/functions.md)
  - [ ] _follow-on:_ a general implicit argument-coercion pass. (`ceil`/`floor`/`mod`/`sign` and text `length`/`lower`/`upper` have since landed in their own slices.)
- [x] **Scalar string / text functions** — PG's string surface as built-ins with code-point semantics (`length`/`substr`/`lpad`/`btrim`/`replace`/`translate`/`repeat`/`strpos`/`split_part`/`encode`/`decode`/`quote_*`/…). → [string-functions.md](spec/design/string-functions.md)
  - [ ] _follow-on:_ full-Unicode `initcap` word classification + non-ASCII titlecasing; keyword-aware `quote_ident`; a `text::bytea` cast + `length`/`octet_length`/`bit_length` `bytea` overloads; per-character cost metering for `lpad`/`rpad`/`repeat` (the §13 cost-ceiling path; the `54000` hard cap is the current backstop).
- [x] **Named + optional (DEFAULT) function arguments** — PG named notation `f(name => value)` + DEFAULT params; `make_interval`, then `make_timestamp`/`make_timestamptz`; `VARIADIC` (landed as **AF6** with the array type). → [functions.md §11](spec/design/functions.md)
  - [ ] _follow-on:_ general non-integer DEFAULT values (no consumer yet — built-ins use overloads or `make_interval`-style 0-defaults); user-defined-function defaults (jed has no UDFs).

---

## The type system as the product (the differentiator, §4)

> `boolean`, `text` (collation `C` → linguistic UCA), `decimal`, `timestamp`/`timestamptz`,
> `date`, `interval`, `bytea`, `uuid`, `f32`/`f64`, **`json`/`jsonb`**, and the `array`,
> `range`, and composite containers are all done. What remains: per-type cast/function
> follow-ons, the JSON `0A000` follow-ons, and the composite-container narrowings.

- [x] **`text` + collation** — UTF-8 code-point order (type code 4), text PK/index/UNIQUE via `text-terminated-escape`; **linguistic collation** landed end-to-end: jed-owned UCA executor, `COLLATE` / per-column / per-db default, collated keys, the reference-only/vendored-tier pivot (`format_version 18`, real Unicode-17 root + `es`), and the host-loaded `JUCD` Unicode-data bundle (`db.LoadUnicodeData`). → [types.md §11](spec/design/types.md), [collation.md](spec/design/collation.md), [encoding.md §2.4](spec/design/encoding.md)
  - [x] **`varchar(n)` length limits** — a single-word `varchar(n)` / `string(n)` max-length typmod (the 2nd parameterized type), counted in code points; over-length assignment traps `22001` (with PG's trailing-space-truncation exception), explicit `::varchar(n)` cast silently truncates; `1 ≤ n ≤ 10485760` else `22023`; `format_version` 22 (text column/field `u32 varchar_max_len` typmod slot). → [types.md §15](spec/design/types.md)
    - [ ] _follow-on:_ two-word `character varying(n)` (single-word-type parser narrowing); `char(n)`/`character(n)` (blank-padded); `varchar(n)[]` element typmod (the `numeric(p,s)[]` narrowing); text `||`, `substring`. _(Runtime non-literal text→T casts + `length`/`lower`/`upper` have landed.)_
  - [ ] _follow-on:_ further locale/feature expansion (curated tailorings, nondeterministic collations, `LIKE` under non-`C`, CLDR `shifted`, CJK tier-3 data) — **possibilities, not scheduled work** ([collation.md §14](spec/design/collation.md)).
- [x] **Exact `decimal`** — *the* headline type: sign+coefficient+scale, round-half-away (settles §8), PG result scales, finite-only (documented divergence), decimal PK/index/UNIQUE via `decimal-order-preserving`; `round`/`ceil`/`ceiling`/`floor`/`trunc(x[,n])`, `gcd`/`lcm`/`width_bucket`, and the exact-numeric transcendentals `sqrt`/`ln`/`exp`/`log`/`log10`/`log(b,x)`/`power`/`pow`. → [decimal.md](spec/design/decimal.md), [encoding.md §2.5](spec/design/encoding.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods; mixed integer/decimal transcendental arguments (`power(2.0, 3)` needs an explicit cast today); per-work cost metering for the transcendentals (one `operator_eval` per call today).
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, `±infinity` first-class, timestamp PK; the host-loaded `JTZ` tz database + `AT TIME ZONE`; `date_trunc`/`EXTRACT`/cross-family casts in a zone + an observable session `TimeZone` slot. → [timestamp.md](spec/design/timestamp.md), [timezones.md](spec/design/timezones.md)
  - [ ] _follow-on:_ `date_part` (float8), `to_char`/`to_timestamp`, `age`, `EXTRACT(julian …)`; a separate `time` type; **text⇄datetime casts** + **session-zone rendering** of `timestamptz`; `timestamp(p)` precision typmods ([timezones.md §9](spec/design/timezones.md)).
- [x] **`date`** — calendar date (i32 days), strict ISO literals + BC + `±infinity`, date PK (type code 16); date arithmetic (`date ± int`, `date − date`, `date ± interval`). A strict island — no implicit compare to timestamp. → [date.md](spec/design/date.md)
  - [ ] _follow-on:_ runtime text→date cast; clock-relative literals (`today`/`tomorrow`/`now`/`epoch`); remaining date functions (`make_date`, `date_part`, `current_date`). → [date.md §6](spec/design/date.md)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, type code 11, interval PK/index/UNIQUE/FK/GIN via the 16-byte `interval-span-i128` key. → [interval.md](spec/design/interval.md), [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ CAST to/from interval; ISO-8601 `P…` + SQL-standard input; field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `justify_*`/`EXTRACT`/`age`.
- [x] **`bytea`** — variable-width bytes, unsigned order, `\x`-hex literals (`22P02`), type code 7, bytea PK/index/UNIQUE via `bytea-terminated-escape`. → [types.md §13](spec/design/types.md), [encoding.md §2.6](spec/design/encoding.md)
  - [ ] _follow-on:_ traditional escape input (`\nnn`); bytea⇄other casts; binary functions (`length`, `||`, `substring`, `encode`/`decode`, `get_byte`).
- [x] **`f32` + `f64` (IEEE 754)** — two-width promotion tower, the first types narrowly exempted from byte-identity (the `R` tolerant compare + exception ledger), type code 12; **float in a PK/index** (`float-order-preserving` key, every scalar now keyable, only `composite` stays `0A000`); the float math functions. → [float.md](spec/design/float.md), [determinism.md](spec/design/determinism.md)
  - [ ] _deferred:_ the `width_bucket(value, thresholds[])` array-threshold variant.
- [x] **`json` / `jsonb` + SQL/JSON** — the committed XL headline feature: all non-deferred slices across all three cores, oracle-clean; spec'd across [json.md](spec/design/json.md), [jsonpath.md](spec/design/jsonpath.md), [json-sql-functions.md](spec/design/json-sql-functions.md), [json-table.md](spec/design/json-table.md); type codes 18/19/20, one `format_version` bump (v18→v19).
  - [ ] _follow-ons (deferred `0A000`, hoisted from the done slices):_ the string-**dictionary builder** (opens the [json.md §3](spec/design/json.md) door); `jsonb`-as-PK/index ([encoding.md §2.13](spec/design/encoding.md)); GIN **`jsonb_ops`** opclass for `@>`/`?`; `JSON_TABLE` explicit `PLAN` (T2); `ON ERROR/EMPTY DEFAULT <expr>` (S3); the remaining **jsonpath** surface (`like_regex` → Pike VM, item methods `.type()`/`.size()`/`.double()`/…, arithmetic, `vars`/`silent` args, the `_tz` query-function variants — P2/P3); the **verbatim-`json`** SRF / accessor variants (`json_array_elements[_text]`, `json_each[_text]`, the `->`/`#>` json overloads); `jsonb_set_lax`; `row_to_json`; in-aggregate `ORDER BY` for `json[b]_agg`.
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — COMPLETE (S0–S6): the open `Type { Scalar | Composite(catalog-ref) }`, `CREATE`/`DROP TYPE`, nested + recursive types, storable composite column + recursive codec (`format_version` 9), `ROW(…)`, field access, element-wise compare/ORDER BY/DISTINCT/GROUP BY. Named composites only. → [composite.md](spec/design/composite.md)
  - [ ] _still narrowed (relaxable later):_ `INSERT … SELECT` / `UPDATE` of a composite column; composite `PRIMARY KEY`/index/`UNIQUE` (`0A000` — key encoding authored, unexercised); `DEFAULT` on a composite column; runtime non-literal text→composite + `composite::text` + anonymous `ROW(…)::type` casts; the nested `ROW(ROW(…),…)`-into-column constructor.

---

## Relational depth + constraints

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
- [x] **`DEFAULT` (literal + expression)** — literal coerced once at CREATE TABLE; non-constant `DEFAULT <expr>` (`uuidv7()`, `1 + 1`) stored as text + evaluated per row through the entropy/clock seam (`format_version` 8). → [constraints.md §2](spec/design/constraints.md)
  - [ ] _follow-on:_ `UPDATE ... SET x = DEFAULT` and `INSERT ... DEFAULT VALUES`.
- [x] **Composite `PRIMARY KEY`** — table-level `PRIMARY KEY (a, b, …)`, key bytes = members' concatenated encodings. → [constraints.md §3](spec/design/constraints.md)
  - [ ] _follow-on:_ composite point-lookup / prefix pushdown (a composite-PK table full-scans today — an optimization slice with its NoREC obligation).
- [x] **`FOREIGN KEY` constraints** — column-/table-level `REFERENCES`, composite + self-reference, same-type pairing (`42804`), MATCH SIMPLE, enforced at four write sites (`23503`), `format_version` 11. → [constraints.md §6](spec/design/constraints.md)
  - [ ] _follow-on:_ the referential **actions** `ON DELETE/UPDATE CASCADE | SET NULL | SET DEFAULT` (parse but `0A000` today); `MATCH FULL`; a **backing index** on the child FK columns (the parent-side check full-scans children today); FK type pairing relaxed to PG's comparable-types; `ALTER TABLE … ADD/DROP CONSTRAINT`.
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — non-unique on-disk B-trees, maintained in the two-phase pass; the planner index-bounds a base scan on an access predicate; `format_version` 5. → [indexes.md](spec/design/indexes.md)
  - [x] **Index ranges + multi-column prefix bounds** — an index-bounded SELECT scan now binds a B-tree **access predicate**: a maximal **equality prefix** on the leading key columns plus an **optional range** on the next column (`v > 5`, `a = 1 AND b = 2`, `a = 1 AND b > 3`). Equality-prefix columns may be any width (including collated text, skipped by known byte length); the range column + trailing columns stay fixed-width. Caps `query.index_range` / `query.index_prefix`; all three cores, no format bump. → [indexes.md §5.1](spec/design/indexes.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ index scans for UPDATE/DELETE (keep PK pushdown today); LIMIT-streaming combination; a variable-width range/tail column (self-delimiting skip, not fixed width); expression/ordered/partial keys; `IF NOT EXISTS`. (All scalar key types are now encodable; only the recursive `composite` container stays a `0A000` key.)
- [x] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* via a type-generic operator-class seam: the **`array_ops`** opclass (one entry per distinct non-NULL element, `format_version` 12's `index_kind` byte, a `gin_entry` cost unit) accelerating `@>`/`&&`/`= ANY(col)`/array `=` for SELECT + GIN-bounded UPDATE/DELETE, over the fixed-width key-encodable element types. → [gin.md](spec/design/gin.md)
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows) / `IN` over a scalar list; the **remaining** element types — the VARIABLE-width keyables (`text[]`, `bytea[]`, `decimal[]`) need GIN term framing; `float[]` and `interval[]` are now UNBLOCKED (fixed-width keys landed) but each is its own slice — plus composite-element arrays; multi-column GIN; correlated / array-column query operands; the ordered-index equality bound for UPDATE/DELETE; the LIMIT-streaming combination; posting-list run compression; the **`jsonb_ops`** opclass and a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints** — a third index *kind* (`index_kind = 2`) whose payoff is PG exclusion constraints (`EXCLUDE USING gist (col WITH op)`, `23P01`); an operation-deterministic R-tree (a structural divergence — jed's own tree bytes), the `range_ops` + fixed-width scalar-`=` opclasses, multi-column GiST; `format_version` 21. → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ the `EXCLUDE … WHERE (predicate)` partial form; `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique index; `ALTER TABLE … ADD CONSTRAINT … EXCLUDE`; **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape.
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <items>` evaluated after validation before any write; the PG-18 `old.`/`new.` row-version qualifiers landed. → [grammar.md §32](spec/design/grammar.md)
  - [ ] _follow-on:_ the `WITH (OLD AS o, NEW AS n)` aliasing form; `old.*`/`new.*`.
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] }`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter; two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT` (with the `UPDATE` `SET = DEFAULT` follow-on); `INSERT INTO t AS alias`; the partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed, but the conflict-path re-key is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [ ] **Temporary tables** — `CREATE [SHARED] [TEMP|TEMPORARY] TABLE` (+ `DROP`): relations that make **zero writes to the database file** (held outside the serialized `Snapshot`, no `format_version` bump), bounded by a deterministic storage budget to keep the untrusted-SQL guarantee (§13). Namespace precludes overlaps (`42P07`); new code `54P03 temp_storage_limit_exceeded`; `allow_ddl` splits into `allow_ddl` / `allow_temp_ddl` / `allow_shared_temp_ddl`. **Landed:** slices 1–2 (session-local + database-wide shared with the two-root commit), CREATE/DROP INDEX on a temp table, serial/IDENTITY, composite-typed columns; and the **temp-blockstore slice** — **session-local** temp now rides a per-domain in-RAM `MemoryBlockStore` + pinned pager (like an in-memory database) with **within-session free-list compaction** (`maybe_compact`, watermark-gated, so a never-reopened store no longer leaks a page per commit) and a **page-based** `54P03` budget (committed `page_count × page_size`) — all 3 cores, result/cost/byte-neutral. **Open:** **shared temp onto a MemoryBlockStore** (core-owned storage + publish decoupling, temp-tables.md §14) and **slice 3 — spill-to-disk** (now a temp-`BlockStore` swap + bounded pool, the flip already put temp on the seam). → [temp-tables.md](spec/design/temp-tables.md) _(size: L; deps: session model (done), storage seam (done))_
  - [ ] _follow-on:_ `ON COMMIT DELETE ROWS`/`DROP`; `IF NOT EXISTS`; `CREATE TEMP TABLE … AS SELECT`; FKs among same-kind temp tables; temporary views. → [temp-tables.md §14](spec/design/temp-tables.md)

---

## Query planner / optimizer

> The planner is a **deterministic rule engine**: it pattern-matches the WHERE shape to pick an
> access path (PK bound → first-column index equality → GIN → GiST → full scan) and runs joins as
> left-deep nested loops in FROM order — no cost-based choice, no statistics, no join reordering.
> `EXPLAIN` (above) now makes those choices inspectable + corpus-assertable, the substrate for this
> work. **The load-bearing constraint:** cost is **observable and a cross-core contract** (§8; the
> `# cost:` corpus directive), so (a) any plan change that changes which plan runs changes the metered
> cost — it must recompute *identically* in all three cores and re-pins the affected `# cost:` entries;
> (b) a cost-*based* planner is admissible **only** if its estimator is itself a spec'd, deterministic,
> cross-core-identical artifact (like the cost schedule) — then cost-based plan choice *extends* the §8
> contract rather than breaking it; (c) some textbook rewrites (constant folding, CSE, short-circuit)
> are **not** cost-neutral here — they drop `operator_eval` charges — so each needs an explicit cost
> decision, not a silent apply. Every optimization is a vertical slice carrying a **NoREC relation**
> (the standing §7 obligation — the sweep does not discover new optimizations).

### Rule-based extensions (results-identical, no statistics)

- [x] **Index-nested-loop join** — ✅ **landed** (`query.index_nested_loop`, all three cores). A
  cross-relation join key (`a JOIN b ON b.pk = a.x`, from the ON or the WHERE) now binds the inner
  relation's scan to a per-outer-row point/range lookup by feeding the sibling column in as the
  bound's source — the same bounded-scan machinery as correlated-subquery pushdown
  (query.correlated_pushdown), with the inner re-materialized per left row (like a correlated
  `LATERAL`). Turns O(N·M) into O(N·log M). PK + leading secondary-index bounds; gated to the
  right/nullable side of an INNER/CROSS/LEFT join (RIGHT/FULL preserved sides keep the full scan);
  EXPLAIN surfaces `Index-nested-loop PK/Index bound`. → [cost.md §3](spec/design/cost.md) "bounded
  scan / index-nested-loop", `spec/conformance/suites/joins/index_nested_loop.test`. _Follow-ons:_
  combining INL with the two-table streaming top-N join (`join_pk_ordered`); GIN/GiST sibling bounds.
- [x] **`OR` / `IN`-list → merged point lookups** — ✅ **landed** (`query.or_in_point_lookup`, all
  three cores). A top-level `OR` is never descended by the contiguous point-lookup bound, and `pk IN
  (1,2,3)` desugars to `pk = 1 OR pk = 2 OR pk = 3` (grammar.md §20) — a shape that full-scanned. A
  disjunction of **equalities on one key column** (the PK, or a leading B-tree secondary-index column)
  now lowers to a **union of point probes** over a de-duplicated, sorted key set (a bitmap-OR), the
  whole WHERE unchanged as the residual filter. Cost = the SUM of the per-probe bounded scans (the
  cross-core §8 contract). A **last resort** (fires only where no contiguous PK/index/GIN/GiST bound
  applies, so no existing plan/cost moves); UPDATE/DELETE lower only on the PK; every single-table
  streaming/columnar/vectorized fast path gates OFF a point-set bound (`needsEagerScan`) so it is
  interpreted in exactly one place. EXPLAIN surfaces `PK point set: <col> in (…)` / `Index point set:
  using <name>`. → [cost.md §3](spec/design/cost.md) "OR / IN-list",
  `spec/conformance/suites/query/or_in_point_lookup.test`. _Follow-ons:_ **range disjuncts** in the
  union (`pk = 1 OR pk BETWEEN 10 AND 20` — a mix of point and range probes); intersecting an IN-list
  with a co-present range conjunct (`pk IN (1..9) AND pk > 4`); a secondary-index point-set for
  UPDATE/DELETE (rides on the index-scans-for-DML item).
- [ ] _already tracked in their home sections (all planner follow-ons):_ **index scans for
  UPDATE/DELETE** and the **LIMIT-streaming + index-bound** combination (the Secondary-indexes item;
  index **ranges** + **multi-column prefix** bounds have since landed — indexes.md §5.1);
  **composite-PK prefix pushdown** (the Composite `PRIMARY KEY` item); a **hash-join operator** (the
  spill item — nested-loop is the only join today); the **ORDER BY + LIMIT top-k** heap (bench-driven
  perf). Each is a rule-based, results-identical win.

### Cost as a plan input (the strategic investment — Path B)

- [ ] **Plan-time cost estimator** — estimate the same cost units the runtime meter charges
  (`page_read`/`storage_row_read`/`row_produced`/…) for each candidate plan and pick the cheapest,
  instead of today's structural tie-breaks (lowest index name, FROM order). Authored as a **spec'd,
  cross-core-identical, deterministic artifact** (the §8 discipline the runtime schedule already
  follows) so plan choice stays byte-identical across cores. The prerequisite for cost-based selection
  and the `EXPLAIN` `est_rows`/`est_cost` columns (the EXPLAIN follow-on above). _(size: L–XL; ×3 cores)_
- [ ] **Table statistics** — the estimator's inputs. Start with a **transactional per-table row count**
  (cheap; deterministic — it rolls back with its transaction like the `nextval` counter,
  [determinism.md §5](spec/design/determinism.md)). Per-column distinct-value counts / histograms are a
  later step, computed by a spec'd pass over the (deterministic) data so they stay cross-core-identical.
  _(size: M row-count / L histograms)_
- [ ] **Cost-based access-path + join-order selection** — with the estimator + row counts, choose the
  cheapest bound per relation and **reorder the left-deep join** (drive the smaller / more-selective
  relation, enable index-nested-loop) rather than honoring FROM order. Re-pins the affected `# cost:`
  corpus entries (the observable-cost consequence above). _(size: L; ×3 cores; +NoREC)_

### Planner infrastructure

- [ ] **Explicit optimizer-pass structure** — the planner is fused pattern-matching in `planSelect`
  today. Split it into logical-plan → rewrite-rules → physical/access-path selection so each
  optimization is a discrete, testable rule (the "boring, explicit, small modules" stance, §10). A
  refactor across all three cores; do it once ≥2–3 rules would share it. _(size: L; ×3 cores)_
- [ ] **Predicate pushdown + simplification** — push WHERE conjuncts into derived tables / CTEs /
  through joins to the earliest relation, and detect contradictions (`x > 5 AND x < 3` → a provably
  empty scan). **Caveat:** plan-time **constant folding** / CSE removes `operator_eval` charges and so
  changes the observable cost — each such rewrite needs an explicit cost decision (the framing above),
  not a silent apply. _(size: M–L; ×3 cores; +NoREC)_

---

## Storage maturation (§9)

> Can lag the feature work until write volume makes whole-image rewrites costly. These items
> are also the path to a **larger-than-RAM file that does not fall over** (CLAUDE.md §9): no
> full-residency assumption above the storage seam.

- [x] **P6.1–P6.4** — incremental COW commit = page-backed B-tree (`format_version` 2, meta-slot root swap); free-list / page reclamation (reconstruct-on-open); the logical `page_read` cost unit; the buffer pool / demand paging (bounded leaf cache, CLOCK eviction, `cache_pages` budget). → [storage.md §4/§6](spec/design/storage.md), [pager.md](spec/design/pager.md)
  - [ ] _follow-on (where the watermark does real work):_ continuous *within-session* reclamation for the **file/in-memory main** domain (return a commit's orphans immediately, paired with file-backed reader sharing) — **the mechanism has landed for temp domains** (periodic ~2×-live free-list compaction, `maybe_compact`, watermark-gated; the temp-blockstore slice, temp-tables.md §6) and is built generically so the main domain can opt in via `reclaim_within_session`; on-disk free-list persistence (claim meta offset 28 to skip the open-time reachable-set walk).
- [x] **B+tree reshape — one packed representation, one storage path** ✅ COMPLETE (`format_version` 24) — the
  B-tree → **B+tree** pivot (records leaf-only; interior pages a record-free separator skeleton →
  far higher fan-out / shallower trees), absorbing the PAX Stage-4 **per-column leaf null bitmap**
  (previously earmarked as its own v24 bump; the Stage-4 **text dictionary** stays a deferred door
  behind the new per-region flags byte), then retiring `Decoded` as a residency form and backing
  in-memory + temp-table stores with a `MemoryBlockStore` through the pager — one read path
  everywhere. One format bump (B1); the cost values re-baselined once; every golden regenerated.
  → [bplus-reshape.md](spec/design/bplus-reshape.md) _(size: XL; §9)_
  - [x] **B0 — spec** ✅ (the decision doc, on master).
  - [x] **B1 — the B+tree + the v24 leaf regions (the format bump)** ✅ all four implementations:
    copy-up leaf / push-up interior splits (+ the pinned degenerate `N = 2 → m = 1` split and the
    interior merge-abandon guard — a legal `N = 0` interior, pinned by the new
    `max_sep_table.jed` golden), end-offset directories, region flags byte + fixed-width null
    bitmap / dense untagged slots + variable zero-span NULLs, `record_size` restated
    (`key_len + Σ value_size`), `RECORD_MAX` value kept; all 58 goldens regenerated; corpus
    green both storage modes with **zero corpus cost drift** (per-core split-shape pins moved:
    268/278/156/100 → 258/265/143/105).
  - [x] **B2 — the Packed simplification** ✅ — folded into each core's B1 port (as the doc
    anticipated: B1 carried the packed read machinery over the new encoding, so B2 was pure
    deletion): the interior-`Decoded`-with-records case, the row-major record codec
    (`encode_record`/`decode_record`/`decode_record_lazy`), the in-order-predecessor delete
    (`max_kv`), and the interleaved separator-emission scan logic are gone from all three cores.
  - [x] **B3 — `MemoryBlockStore` + pinned pool** ✅ (in-memory databases): an in-memory database
    is a `MemoryBlockStore` seeded with the empty from-scratch image, demand-paged through the
    same pager + Packed path as a file (pinned/unbounded pool); the eager whole-image
    `from_image` loader, the `persist` in-memory no-op, and the separate in-memory constructor
    are deleted — one loader, one commit path (the file commit minus durability). **The reshape
    narrowed temp-table stores to fully resident; the temp-blockstore slice RETIRED that for
    session-local temp** — it now rides a per-domain `MemoryBlockStore` + pinned pager with
    within-session compaction (temp-tables.md §6), the store move this narrowing deferred. **Shared**
    temp still rides the `Decoded` writer-scratch arm B4 keeps anyway (a follow-on — core-owned storage).
  - [x] **B4 — retire `Decoded` + the demand-fault backstop** ✅: the post-commit residency flip
    (committed clean leaves demote to `OnDisk` at publish and fault back Packed through the pool
    — `Decoded` survives only inside an uncommitted writer); `Unfetched` values carry their own
    resolution handles (column-type ref + weak pager handle) and the evaluator's column access
    resolves a touched-set miss **on demand, unmetered** (the backstop — deterministic rows,
    never a NULL-fold; the touched set stays the cost basis + prefetch); the two-form
    masked/unmasked reconstruction seam is deleted.
- [ ] **File compaction / shrink (return space to the OS)** — ⏳ **approach decided, not built.** The free-list recycles dead space for jed but `page_count` is a monotonic high-water, so the file is grow-only. Decided mechanism: a **host-invoked compaction** that re-serializes the committed snapshot through the from-scratch `to_image` serializer into a fresh file + atomic swap (the `create` temp-file + fsync + rename recipe), reclaiming all dead space + defragmenting (the SQLite `VACUUM` / PG `VACUUM FULL` flavor) crash-safely. Explicit / host-invoked, gated on the reader-liveness watermark; needs nothing new at the storage seam. A lighter in-place trailing-free truncation stays open as a cheaper partial complement. → [storage.md §6](spec/design/storage.md) _(size: M–L; deps: P6.2; §9)_
- [ ] **Streaming + spill-to-disk operators** — bound blocking operators (`ORDER BY`, hash `JOIN`, `GROUP BY`/aggregate, `DISTINCT`) by a memory budget and **spill to disk** when exceeded, so a query over larger-than-RAM data never materializes its whole input/output in memory. **Landed:** the **external merge sort for `ORDER BY`** (a `Sorter` bounded by `work_mem`, spills sorted runs + k-way merges, byte-for-byte identical to the in-memory sort). → [spill.md](spec/design/spill.md) _(size: XL; deps: paged storage; §9/§13)_
  - [ ] **Spilling hash aggregate / `DISTINCT` / hash JOIN** — the remaining blocking operators (spill.md §7). Each needs a *different* algorithm: a partitioned (grace) hash that preserves first-occurrence order for aggregate/DISTINCT, and — for hash JOIN — a hash-join operator first (jed joins are nested-loop today), then grace-hash spill to bound the build side. _(size: L–XL each)_
- [ ] **True streaming result cursor** — make `Rows` a **pull source** so the non-blocking single-table pipeline yields row-at-a-time and blocking operators buffer-then-stream their output, instead of materializing the whole result before the caller sees a row (today `exec_select_plan` returns a full `Vec<Vec<Value>>`). PG/SQLite-faithful: a pull executor + **PG-faithful cursor snapshot pinning** (the cursor registers in the reader-liveness watermark, releases on drain/close/Drop). Cost is **byte-identical under full drain** (the harness drains), so no new corpus capability; the binding cross-core rule is the **mirrored streaming loop** (the `max_cost` abort point stays identical). Distinct from the operator-spill item above — that bounds blocking *input*; this bounds *output* and adds the pull cursor (the VDBE-forward prerequisite). → [streaming.md](spec/design/streaming.md) _(size: XL; deps: pager (done), spill (done), watermark (done); §9/§13)_
  - [x] **S1 — the `Cursor` seam (no observable change):** `exec_select_plan` returns a `Cursor` (only a `Buffered` shape, wrapping today's materialized `Vec`); `Rows` delegates `next`/`column_names`/`cost`/`close`. Pure refactor — results, cost, goldens byte-unchanged.
  - [x] **S2 — the pull B-tree scan cursor:** convert the scan from push (`scan_range(visit)`) to a pull cursor (frame stack over the persistent map) in Rust/Go, a generator in TS.
  - [x] **S3 — stream the non-blocking pipeline + snapshot pinning** ✅ (all three cores): the `query()` → `Rows` single-table no-blocking-operator read (the PK-ordered / LIMIT-short-circuit shape, gated by the shared `streaming_scan_eligible`) is now a lazy `Streaming` cursor — scan-cursor (S2) → resolve → WHERE → project, one row per `next`, bounded peak memory, early-exit. The cursor owns a frozen snapshot (Rust: a snapshot `Engine` sharing the seam via `Rc` + the lifetime gauge; Go/TS: captured snapshot engine sharing the seam by reference) and registers its version in the reader-liveness watermark (`reader_pin`, released on `close`/drop). `execute()` stays materialized (the corpus drives it, byte-unchanged); a mid-drain error surfaces during iteration (Rust `Rows::error()` / Go `Rows.Err()` / TS throws). Per-core unit-tested (`query()`==`execute()` rows+cost under full drain, early-exit charges less, snapshot pin + watermark, mid-drain abort). _Follow-ons: index-order/sort/join still buffer (S4); prepared-statement streaming; a `Database::query` watermark on the bare single-handle path._
  - [x] **S4 — lazy output from blocking operators** ✅ (all three cores): the `query()` → `Rows` blocking read (a non-PK `ORDER BY`, `DISTINCT`, aggregate/`GROUP BY`, window, or join) is now a lazy `Buffered` cursor (`BufferedScan` / `bufferedScanCursor` / a `bufferedRows` generator). The seam is `exec_select_emit`, extracted from `exec_select_plan`: it runs the blocking part and returns an `Emitter` — a windowed `Buffer` (`Project` evaluates the projection list on emission; `Identity` is the pre-projected DISTINCT output) or a `Final` result (the special input-streaming paths, already projected+charged). `exec_select_plan` drives it eagerly (the materialized `execute()` path, byte-unchanged); the lazy cursor owns a frozen snapshot engine (sharing the seam + lifetime gauge, like S3), runs the blocking part on its **first pull** (so a `54P01`/`57014`/arithmetic trap surfaces during iteration, not at `query()`), then yields the buffer one row at a time — bounding peak output memory + skipping the projection of un-pulled rows (top-N over the buffer). Byte-identical rows+cost under full drain; per-core unit-tested (buffered matches eager, buffered early-exit charges less, snapshot pin+watermark, mid-drain abort). _Follow-ons: top-level set-op / `WITH` streaming (✅ S6 below); lazy `exec_streaming_sort` output via `SortedRows` (✅ S7 below); prepared-statement streaming; a `Database::query` watermark on the bare single-handle path._
  - [x] **S5 — lazy small-inline-column decode — SUPERSEDED / promoted out (not built).** Planning surfaced that the narrow "decode-in-place" S5 fights jed's architecture (the decoded `Row` is detached and **deep-cloned per scan** out of the resident node — there is no shared page tuple to decode in place like PG/SQLite), so it is a wash-to-slight-loss except on untouched deep-tree columns. Promoted to its own storage-core reshape — see **Lazy record decode** below.
  - [x] **S6 — lazy DEFERRED set-operation / `WITH`** ✅ (all three cores): the `query()` → `Rows` path now serves a top-level set operation (`UNION`/`INTERSECT`/`EXCEPT`) or pure-query `WITH` through a lazy **deferred** cursor (`DeferredResult` in Rust, `deferredCursor` in Go, an inline `RowSource` in TS), wired after the `Buffered` lane (`try_deferred_query` / `tryDeferredQuery`). These outputs are already projected + charged (no per-row top-level projection to defer), so the win is **lazy-yield only**: the cursor resolves the output column names by planning-only up front (unmetered/deterministic → names match the deferred run), owns a frozen snapshot engine (§5), and on its **first pull** runs the eager `run_set_op` / `run_with` **verbatim** (rows + total cost byte-identical to `execute()` by construction — no re-implemented execution path), then yields the result one row at a time. A `54P01`/`54P02`/cancellation/arithmetic trap surfaces during iteration (the run is on the first pull); the snapshot pin registers in the watermark like S3/S4; because the whole query runs on the first pull, an early exit charges the **same** as a full drain (lazy-yield only). A data-modifying `WITH` (a write, `stmt_is_write`) / a `nextval`/`setval` set-op/`WITH` is NOT taken — it falls back to the materialized dispatch (takes the write gate). Per-core unit-tested (matches eager across every set-op kind + recursive/aggregate/join `WITH`, runs-fully-on-first-pull cost, snapshot pin + watermark, mid-drain abort, data-modifying-`WITH` fallback). No `format_version` bump; corpus green by construction. → [streaming.md §7](spec/design/streaming.md) _Remaining streaming follow-on: lazy `exec_streaming_sort` output (✅ S7 below); prepared-statement streaming; the bare-handle `Database::query` watermark._
  - [x] **S7 — lazy `exec_streaming_sort` output** ✅ (all three cores): the streaming external sort buffered its **input** through the `Sorter` already, but its **output** was an `Emitter::Final` (the full windowed result `Vec` built + charged up front on the first pull, so an early exit charged the same as a full drain). Now `exec_streaming_sort` runs the blocking part (scan + sort + `OFFSET` skip) and returns an `Emitter::Sorted` — the `SortedRows` pull iterator positioned at the first output row + the windowed `remaining` count — and the emitter drive (eager `exec_select_plan`; lazy `BufferedScan` / `bufferedScanCursor` / `bufferedRows`) pulls one sorted row per emission, charging `row_produced` + the projection **per pull**. So the output `Vec` is never built and a caller's early exit skips the `row_produced` + projection of the rows it never pulls (the top-N-over-the-sort win the `Final` form could not offer). The collation-aware path wraps its in-memory-sorted survivors as an in-memory `SortedRows`, flowing through the same lazy emitter. An early exit / `LIMIT`-stopped merge drops the `SortedRows`, whose `Merger` cleanup releases undrained spill runs (Rust `Drop` / Go cursor `close` / TS generator `finally`). Byte-identical rows+cost under full drain (the sort is unmetered, spill.md §6); per-core unit-tested (sorted matches eager over an `OFFSET`/`LIMIT`/projection-expr/filter/empty battery, sorted early-exit charges less, spilling-merge streams lazily + leaves no temp file on early exit). No `format_version` bump; corpus green by construction. This was the last `exec_select_emit`-path output-laziness follow-on. → [streaming.md §7](spec/design/streaming.md) _Remaining streaming follow-on: the bare-handle `Database::query` watermark (prepared-statement streaming ✅ S8 below)._
  - [x] **S8 — prepared-statement streaming** ✅ (all three cores): a prepared query (`prepare` + `query_prepared` / `QueryValues` / `PreparedStatement.query`) used to **materialize** (run the eager `execute`/`dispatch` path, wrap the `Outcome` in a buffered `Rows`), so it got none of S3/S4/S6/S7's laziness. Each core extracts a shared route-an-already-parsed-AST helper — `query_ast` (Rust `Engine` + `Session`), `queryStmt` (Go `engine` + `Session`), `queryStmt` (TS `Engine`) — holding the streaming / buffered / deferred lane dispatch (+ the `Session`-path autocommit re-pin and watermark pin) that ad-hoc `query()` already used; both `query()` (parse-then-route) and the prepared query (route the prepared AST) call it. So a prepared query now **streams identically to a one-shot one** — same lanes, same early-exit win, same snapshot pin, the `54P01`/`57014`/trap surfacing during iteration (the lazy lanes defer to the first pull). Byte-identical rows + total cost under full drain (the same lanes the corpus's `execute()` already drives → green by construction); per-core unit-tested only (`prepared_query_*` / `TestPreparedQuery*` / `"prepared query …"`: matches-eager across every lane, binds `$N`, early-exit-charges-less, the session-path snapshot pin + watermark, mid-drain cost abort). Cross-core shape: Rust/Go expose a low-level prepared query on both the bare `Engine` and the shared-core `Session` (the latter pins); TS's low-level `PreparedStatement` binds only to a bare `Engine` (its session-bound prepared path is the ergonomic `Statement`, which already streamed). The WASM C-ABI `jed_stmt_query` drains the now-streaming cursor through a new `ok_rows` helper that surfaces a mid-drain error as an `ERROR` buffer (instead of truncating); the Ruby native ext exposes only `jed_execute` (materialized), unaffected. No `format_version` bump. → [streaming.md §7](spec/design/streaming.md) (S8) _Remaining streaming follow-on: the bare-handle `Database::query` watermark._
- [ ] **Lazy record decode** — keep a faulted leaf as its **compact on-disk bytes** and decode each column **on demand** for the query's touched set, instead of materializing every value into an inflated `Value` tree at fault. Generalizes the large-values.md §14 `Unfetched` lazy path from *large values* to *every value* — so it reuses the existing `needs_resolution` / `resolve_columns(row, mask)` plumbing at the four touched-set read sites and the static touched-set cost contract. Three independent wins: (1) lazy column decode (the old S5 goal, now uniform/no-per-type-rule and with no touched-path regression); (2) the per-scan clone drops from a deep `Value`-tree clone to a refcount bump / flat copy; (3) **resident leaf memory drops to `≈ page_size`**, making the buffer-pool byte budget honest (CLAUDE.md §9). Results/cost/byte-neutral (no `format_version` bump; no per-column-decode cost unit), so each core lands green independently. The §3 enabler: B-tree navigation reads only raw-byte keys, so values can go lazy without touching split/merge/descent. Also supplies the VDBE `OP_Column`-over-a-raw-record primitive (streaming.md §3). → [lazy-record.md](spec/design/lazy-record.md) _(size: XL; deps: pager (done), large-values §14 lazy path (done); §9/§13)_
  - [x] **L0 — spec** ✅ ([lazy-record.md](spec/design/lazy-record.md)): the model (universal lazy value deferral), the resident-representation choice ((a) zero-copy block-shared target vs (b) owned-span first step — not a byte contract, per-core idiomatic like CLOCK), the drift-free no-construct decode seam (§6), snapshot/COW/watermark composition, cost/byte-invariance, and the L0–L3 slice sequence. Streaming.md §8 / pager.md §3 / cost.md §3 / CLAUDE.md §9 updated.
  - [x] **L1 — the no-construct decode mode (no observable change)** ✅ all 3 cores on `feat/streaming-cursor` (Rust `24703637`, Go `cde0b429`, TS `9a02df3e`; PUSHED, not master): a `DecodeMode` (Construct/Skip) threaded through `read_inline_body` + the 6 callees so a body's byte extent is found by the SAME cursor-advancing reads without building the value (zero-drift by construction); fixed-width scalars stay eager (§6). Added `inline_body_span` / `inlineBodySpan` (TS returns a zero-copy subarray). Eager path byte-identical; per-core test `inline_body_span_matches_decode` asserts span-advance == construct-advance + span == body bytes over a rich row (every type incl. jsonb/composite/array/range/decimal). Seam-first (the P6.4a move); `rake ci` exit 0.
  - [x] **L2 — defer inline values at fault (form (b), the heart)** ✅ all 3 cores on `feat/streaming-cursor` (Rust `b792c2c2`, Go `51d2a1e5`, TS `462cd353`; PUSHED, not master): `read_value_lazy` produces a deferred `Unfetched::Inline` (owned span, form (b)) for variable-length / structured present values (the `is_spillable` set — text/bytea/decimal/json/jsonb/composite/array/range), found via the L1 `inline_body_span` walk; fixed-width scalars stay eager (§6). `resolve_unfetched` reconstructs a touched one from the owned span (no pager); the cost walk + `mark_chains` treat it as zero units / no chain. The deep-tree clone + the eager decode of untouched columns are gone. Cost/results/goldens unchanged (the corpus runs in-memory/eager `from_image`, green by construction). **One latent gap fixed:** internal index/FK-maintenance write paths read a faulted row's *key* columns directly (not via a touched-set mask) — a key column is always inline, so a new `resolve_inline_columns` (cost-free — owned bytes, no I/O) resolves exactly those at the 3 such sites (CREATE INDEX build, UPDATE old row, DELETE matched row), restoring the pre-L2 picture (pre-L2 this never bit, since key columns are key-size-bounded and so never spilled/eager). Spill codec gains tag 21 for the inline pass-through of an untouched deferred column riding a spilling sort. Per-core `lazy_inline_values` tests: paged-vs-resident rows+cost over a broad query-shape battery, mutation round-trip, read-on-touch corrupt inline body (`XX001` only when touched), spilling-sort carried column. `rake ci` exit 0. No `format_version` bump.
  - [x] **L3 — zero-copy block-shared (form (a), the memory win)** ✅ all 3 cores on `feat/streaming-cursor` (Rust `c93566e8`, Go `5ebe1ce6`, TS `d07a0906`; PUSHED, not master): a deferred inline value now references the faulted leaf's **shared page block** instead of owning a copy of its body. Go/TS were nearly free — `readValueLazy` drops the body copy and keeps the span as a slice / `Uint8Array` subarray view, which keeps the one page block alive under GC and shares it across every deferred value in the leaf. Rust upgraded (b)→(a): `Unfetched::Inline { block: Arc<[u8]>, off: u32, len: u32 }`, one `Arc` per leaf threaded through `read_value_lazy`/`decode_record_lazy` and all three callers (`decode_leaf_node`, `collect_leaf_overflow`, `read_skeleton_node` — interiors share their block with their resident separators too); the fault copies nothing per value and the scan-emit clone is a refcount bump; the spill pass-through (tag 21) writes the span and reloads a degenerate form (a) (a fresh single-body `Arc`, since the block is gone). Resident leaf memory now tracks `≈ page_size` (§9), the honest buffer-pool bound. Cost/results/goldens unchanged (§8) — no `format_version` bump, corpus green by construction. Per-core white-box test proves block-sharing (Rust `Arc::ptr_eq`; Go `cap > len` view, not a copy's `cap == len`; TS same identical `ArrayBuffer`) + resolve-to-eager-value. `rake ci` exit 0 + `rake concurrency:race` green. _Follow-ons (none foreclosed): keys as block slices; in-memory adoption — now scheduled as **bplus-reshape B3** (the `MemoryBlockStore` pager backing, below); a per-column offset cache (SQLite `OP_Column` memoization)._
- [ ] **Bench-driven perf follow-ons** — the measured gaps remaining after the `perf-point-lookup` work (which took `point_lookup_pk` past same-language PG clients in all 3 cores):
  - [ ] **Rust CoW insert deep-clone** — `node_insert` rebuilds a path node with `Vec::clone`, deep-copying every key (`Vec<Vec<u8>>`) + row where Go's `[][]byte` copy is pointer-shallow (why `insert_rollback` is rust 21.6ms vs go 10.3ms). Fix: share entry storage (`Arc<[u8]>` keys / `Arc`-shared rows). Rust-only, no byte or cost change. _(size: M)_
  - [ ] **ORDER BY + LIMIT top-k** — `order_by_limit` full-sorts all 1M rows before slicing (0.76–1.6s vs PG ~20ms). A bounded top-k selection (heap of LIMIT+OFFSET, index-stable tie-break) cuts the sort to ~scan cost. Rows + cost unchanged. The post-reshape bench shows TS +16% on this lane (lazy-reconstruction overhead on the full-sort input) — this item subsumes that residual. _(size: M; ×3 cores)_
  - [ ] **Full-scan materialization** — `full_scan_agg` clones every row into a buffer before aggregating (143–281ms vs PG ~13ms). Streaming aggregation over the scan visitor is the contained first step; the full fix is the spill item above. _(size: M–L)_
- [x] **Large values — overflow pages + compression (TOAST-equivalent)** — large `text`/`bytea`/`decimal`/`json` pushed out-of-line onto overflow-page chains (`format_version` 3), optionally LZ4-compressed first via a deterministic hand-rolled block codec (no third-party dep — a library fails §8 byte-identity). → [large-values.md](spec/design/large-values.md), [lz4.md](spec/fileformat/lz4.md)
  - [ ] _follow-on:_ chain sharing on rewrite (let a rewritten record keep an unchanged value's existing chain — a byte-layout change, lands in all cores + incremental tests together).

---

## Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters + sessions
> + the CLI have landed; OPFS spill + the e2e-in-CI gap remain. Parallelizable with most feature work.

- [ ] **Storage hosts** — the five-method `BlockStore` byte device, host catalog, and decoration layering (encryption codec above the seam, replication tee below) authored in [hosts.md](spec/design/hosts.md). **Landed:** the Node `fs` host, the `FileBlockStore` extraction, and the **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity vs goldens, gated Playwright e2e); Rust/Go inline `std::fs`/`os` in the per-core `Pager`. **Open:** OPFS disk-spill, the e2e in CI. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [x] **Session / shared-handle convergence** ✅ COMPLETE (7a–7d + the concurrent-reader bench) — fold `SharedDb`/`ReadHandle`/`WriteHandle` into `Database` + `Session` so a session *is* the configured concurrency handle, and `Database` mints concurrently-usable sessions (the deferred [session.md §2.4/§10 slice 7](spec/design/session.md) item, now designed). **Decided shape:** full rename (`SharedDb`→`Database`, the old executor handle→`Engine`); unified PG-like sessions (one writable session, lazy gate on first write — `db.read_session()`/`write_session()`/`session()`); file-backed included. All three cores in lockstep; corpus + results byte-identical (no new capability flags); the `activate`/swap is deleted; `Database` keeps a long-lived default session so the single-handle path (and every harness/example/web bridge) is unchanged. **Sub-slices:** **7a** ✅ rename-only (its own commit, green ×3); **7b** ✅ in-memory convergence (envelope `Session`→`SessionState`; the unified `Session` host handle minted by `db.read_session()`/`write_session()`/`session(opts)`, each owning a private `Engine`; the lazy gate; `activate`/swap deleted; migrated the concurrency conformance driver + stress harness + `shared`/`session`/`privileges`/`execute_script`/`lifetime_cost`/`variables` per-core tests; corpus byte-identical 281×3); **7c** ✅ file-backed sessions + the **default-session bridge** (the shared core gained the storage identity + a writer-gate `persist`; `open`/`create` return a `Database` owning a long-lived default `Session` with the `execute`/`query`/`begin`/`commit`/`rollback`/`status`/`execute_script` + envelope delegators; **per-core handle shape** — Go/TS `Database` *is* the safe core, Rust `Database` is a `!Send` wrapper over a separately-named `Send+Sync` `SharedCore` reached via `db.core()`; thread-safe-pager-under-concurrent-faults holds by construction via the `Mutex`-guarded `SharedPaging` + copy-on-write snapshots; **watermark-gated reclamation satisfied trivially** — reconstruct-on-open free-list only, so continuous within-session reclamation + active gating stays the deferred follow-on, transactions.md §8; minted sessions serialize at the file page size for cross-core byte-identity; per-core `file_sessions` tests; no format bump, no new caps. Remaining: the `Database` concurrent-reader bench); **7d** ✅ docs — the six `web/examples/*` topics × {Rust, Go, TS} rewritten to the `Database` handle + delegators (open/create→`Database`; SQL via `db.execute`/`db.query`; sessions minted with `db.session(opts)`, whose `execute`/`query` no longer take a `db` arg; `update`/`view` in Rust/Go, `begin`/`commit`/`rollback` in TS), `web/src/routes/docs/api/*` prose corrected; verified by `vite build` (Shiki) + 42-test Playwright e2e. **Concurrent-reader bench** ✅ (the slice's last item) — the ci-external `concurrent_read` kind ([benchmarks.md §8.1](spec/design/benchmarks.md)): `concurrent_read_pk_r{1,4}` mint N reader `Session`s on one shared `Database` over the resident `small` dataset; the three native cores agree on the partition-folded answer checksum (a new cross-core differential test of the concurrent read path) and scale near-linearly (Rust ~2.8×, Go ~3× at 4 readers; TS single-threaded), proving the §3 lock-free read path. jed-only; PG/SQLite cross-engine + larger-than-pool variants deferred. → [session.md §2.4/§10](spec/design/session.md), [api.md §2.5](spec/design/api.md), [transactions.md §8/§10](spec/design/transactions.md), [benchmarks.md §8.1](spec/design/benchmarks.md) _(size: L; deps: session model (done), shared handle (done), watermark (done))_
- [x] **Database loses its persistent default session** ✅ COMPLETE (all 3 cores + FFI + web) — the reversal of slice-7c's default-session bridge. `Database` no longer owns a long-lived default `Session`; it is the shared core and its bare convenience methods (`execute`/`query`/`execute_script`/`view`/`update` + `prepare`/`*_prepared`) **mint a fresh autocommit session per call and discard it** (committed data persists through the core; no session-local state — open block, vars, `currval`, session-local temp tables — carries across calls). The persistent-connection surface (`begin`/`commit`/`rollback`/`status`/`in_transaction` + every envelope setter/getter: `set_var`/`set_max_cost`/`grant`/`set_time_zone`/seam/temp budgets/…) is **removed from `Database`** and lives **only on an explicit `Session`** (`db.session(opts)`/`read_session()`/`write_session()`). **Rust:** `Database` absorbed the `Send+Sync` `SharedCore` (it is now `Send+Sync+Clone` itself; the public `SharedCore` type and `db.core()` are gone). **TS:** `view`/`update` landed on `Database`/`Session` (the api.ts `Transaction` gained a session-routed commit/rollback hook). **FFI:** the Ruby ext + WASM C-ABI handle is now a persistent `Conn { db, sess }`, so `jed_execute("BEGIN")`…`jed_commit()` still spans calls. **Phase 1** moved every test/bench/harness off `Database`'s persistent surface to an explicit `Session`; **Phase 2** did the restructure. No format bump, no new caps; `rake ci` green ×3. → [session.md §2.1/§2.4](spec/design/session.md) _(size: M)_
- [ ] **(Open question, not scheduled)** low-level direct access API beneath SQL (`getValue("table", key)`) — keep the seam open, don't build yet (§9). _(size: —)_

---

## Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Several items are **ongoing obligations**
> that grow with each feature, not one-shot tasks.

- [ ] **Differential-testing harness** vs the PostgreSQL oracle (§7) — **PARTIAL.** The live-`db` oracle-import tool is built (`scripts/oracle_import.rb`; `rake corpus:import/check`; the override ledger `spec/conformance/oracle_overrides.toml`). *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated** reference provisioning §12 — never auto-provision). SQLite is deliberately not an oracle; mining its sqllogictest corpus for query *shapes* (answers from PG) is the only oracle-adjacent use. _(size: M remaining)_
- [ ] **SQLancer-style metamorphic / generative testing** — **PARTIAL.** Built so far (`scripts/norec_gen.rb`; `rake corpus:norec_sweep`, in `rake ci`): the **NoREC** slice (pushdown vs non-optimizable rewrite must agree), the **TLP** slice (ternary-logic partitioning), and an automatic **reducer** (`scripts/reduce.rb`, ddmin). *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator), aggregate `GROUP BY` TLP (blocked on `COALESCE`/`LEAST`/`GREATEST`), and broader NoREC relations. _(size: M remaining)_
- [ ] **Corpus growth** (ongoing) — keep adding `.test` coverage as each feature lands. Two **standing obligations** (conformance.md §5/§8): (a) on the PG-comparable surface, run `rake corpus:check` and register any intentional divergence in the override ledger; (b) **when you add a query optimization or a new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does not discover new optimizations. (Future index/DISTINCT/aggregate pushdown are not yet covered.)
- [ ] **Benchmark backfill** (ongoing) — grow `bench/corpus` beyond the v1 set (benchmarks.md §11): a join benchmark (needs a second dataset table → `generator_version` bump), GROUP BY aggregate, UPDATE/DELETE throughput, miss-heavy point lookups, text/large-value-heavy rows, `SharedDb` concurrent-reader throughput, cold-open time, durable-commit batch-size sweep. **Standing obligation** (§10): a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected benchmarks before/after. _(size: M, ongoing)_

---

## Language reach: more supported languages (§2)

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
