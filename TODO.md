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
  `depth`/`node`/`detail`/`est_rows`/`est_cost` result set (pre-order, `nosort`), without executing
  the inner statement;
  `ANALYZE` runs it and reports the actual (deterministic) accrued cost + row count on an `Analyze`
  root. Covers read queries + DML (plan-only, never mutates); `ANALYZE` of a write executes + commits.
  The observability substrate for the cost-based planner. → [explain.md](spec/design/explain.md)
  - [x] _follow-on:_ per-node actual cost attribution under `ANALYZE`; a full expression printer for
    the residual filter / projections (currently a `conjuncts=N` count) + exact float-literal bound
    rendering (each needs a determinism-ledger entry); an `EXPLAIN (…)` option list; a
    streaming/buffered/deferred lane tag; the DML touched-set count; `EXPLAIN` of a data-modifying `WITH`.
- [x] **Predicate forms — `IN`/`BETWEEN`/`LIKE`/`CASE`** — plus `ILIKE`, and the regex operators `~`/`~*`/`!~`/`!~*` + `regexp_replace`/`regexp_match` (a hand-written linear-time Pike VM, ReDoS-immune). → grammar.md §20–§23, [regex.md](spec/design/regex.md)
  - [ ] _follow-on:_ LIKE `ESCAPE 'c'`; `SIMILAR TO` (deliberately excluded — the SQL-standard surface); set-returning `regexp_matches` / `regexp_split_to_table`; the Oracle-compat `regexp_count`/`instr`/`substr`/`like`; Unicode-property char classes (`\p{…}`).
- [x] **Scalar functions `abs` / `round`** — first named per-row functions. → [functions.md §9](spec/design/functions.md)
  - [ ] _follow-on:_ a general implicit argument-coercion pass. (`ceil`/`floor`/`mod`/`sign` and text `length`/`lower`/`upper` have since landed in their own slices.)
- [x] **Scalar string / text functions** — PG's string surface as built-ins with code-point semantics (`length`/`substr`/`lpad`/`btrim`/`replace`/`translate`/`repeat`/`strpos`/`split_part`/`encode`/`decode`/`quote_*`/…). → [string-functions.md](spec/design/string-functions.md)
  - [ ] _follow-on:_ full-Unicode `initcap` word classification + non-ASCII titlecasing; keyword-aware `quote_ident`.
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
- [x] **Exact `decimal`** — *the* headline type: sign+coefficient+scale, round-half-away (settles §8), PG result scales, finite-only (documented divergence), decimal PK/index/UNIQUE via `decimal-order-preserving`; `round`/`ceil`/`ceiling`/`floor`/`trunc(x[,n])`, `gcd`/`lcm`/`width_bucket`, and the exact-numeric transcendentals `sqrt`/`ln`/`exp`/`log`/`log10`/`log(b,x)`/`power`/`pow`. → [decimal.md](spec/design/decimal.md), [encoding.md §2.5](spec/design/encoding.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods; mixed integer/decimal transcendental arguments (`power(2.0, 3)` needs an explicit cast today).
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, `±infinity` first-class, timestamp PK; the host-loaded `JTZ` tz database + `AT TIME ZONE`; `date_trunc`/`EXTRACT`/cross-family casts in a zone + an observable session `TimeZone` slot. → [timestamp.md](spec/design/timestamp.md), [timezones.md](spec/design/timezones.md)
  - [ ] _follow-on:_ `to_char`/`to_timestamp`, `age`, `EXTRACT(julian …)`; a separate `time` type; **text⇄`timestamp`/`timestamptz` casts** + **session-zone rendering** of `timestamptz`; `timestamp(p)` precision typmods ([timezones.md §9](spec/design/timezones.md)). (`date_part` (float8) has landed.)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, type code 11, interval PK/index/UNIQUE/FK via the 16-byte `interval-span-i128` key. → [interval.md](spec/design/interval.md), [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ CAST to/from interval; ISO-8601 `P…` + SQL-standard input; field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `justify_*`/`EXTRACT`/`age`.
- [x] **`bytea`** — variable-width bytes, unsigned order, `\x`-hex literals (`22P02`), type code 7, bytea PK/index/UNIQUE via `bytea-terminated-escape`. → [types.md §13](spec/design/types.md), [encoding.md §2.6](spec/design/encoding.md)
  - [ ] _follow-on:_ traditional escape input (`\nnn`); bytea⇄other casts; binary functions (`length`, `octet_length`, `bit_length`, `||`, `substring`, `get_byte`).
- [x] **`f32` + `f64` (IEEE 754)** — two-width promotion tower, the first types narrowly exempted from byte-identity (the `R` tolerant compare + exception ledger), type code 12; **float in a PK/index** via the `float-order-preserving` key; the float math functions. → [float.md](spec/design/float.md), [determinism.md](spec/design/determinism.md)
  - [ ] _deferred:_ the `width_bucket(value, thresholds[])` array-threshold variant.
- [x] **`json` / `jsonb` + SQL/JSON** — the committed XL headline feature: all non-deferred slices across all three cores, oracle-clean; spec'd across [json.md](spec/design/json.md), [jsonpath.md](spec/design/jsonpath.md), [json-sql-functions.md](spec/design/json-sql-functions.md), [json-table.md](spec/design/json-table.md); type codes 18/19/20, one `format_version` bump (v18→v19).
  - [ ] _follow-ons (deferred `0A000`, hoisted from the done slices):_ the string-**dictionary builder** (opens the [json.md §3](spec/design/json.md) door); `jsonb`-as-PK/index ([encoding.md §2.13](spec/design/encoding.md)); GIN **`jsonb_ops`** opclass for `@>`/`?`; `JSON_TABLE` explicit `PLAN` (T2); `ON ERROR/EMPTY DEFAULT <expr>` (S3); the remaining **jsonpath** surface (`like_regex` → Pike VM, item methods `.type()`/`.size()`/`.double()`/…, arithmetic, `vars`/`silent` args, the `_tz` query-function variants — P2/P3); the **verbatim-`json`** SRF / accessor variants (`json_array_elements[_text]`, `json_each[_text]`, the `->`/`#>` json overloads); `jsonb_set_lax`; `row_to_json`; in-aggregate `ORDER BY` for `json[b]_agg`.
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — COMPLETE (S0–S6): the open `Type { Scalar | Composite(catalog-ref) }`, `CREATE`/`DROP TYPE`, nested + recursive types, storable composite column + recursive codec (`format_version` 9), `ROW(…)`, field access, element-wise compare/ORDER BY/DISTINCT/GROUP BY. Named composites only. → [composite.md](spec/design/composite.md)
  - [x] _composite-as-key landed:_ a composite-typed `PRIMARY KEY` / ordered secondary index / `UNIQUE` column of an all-keyable-field type — the third container key (`composite-field-slots`, [encoding.md §2.15](spec/design/encoding.md)); point-lookup pushdown deferred like the other container keys.
  - [ ] _still narrowed (relaxable later):_ `INSERT … SELECT` / `UPDATE` of a composite column; an **array-of-composite** field as a key + a composite **FK** pairing (the remaining `0A000` key axes); `DEFAULT` on a composite column; runtime non-literal text→composite + `composite::text` + anonymous `ROW(…)::type` casts; the nested `ROW(ROW(…),…)`-into-column constructor.

---

## Relational depth + constraints

- [x] **Subqueries** — uncorrelated scalar, `[NOT] IN (SELECT …)`, `[NOT] EXISTS`, correlated, subqueries in UPDATE/DELETE, `$N` inside a subquery, derived tables, a `VALUES` body, `LATERAL`, `x op ANY/ALL(SELECT …)`. → [grammar.md §26/§42/§44](spec/design/grammar.md)
  - [ ] _follow-on:_ a correlated `GROUP BY` key (`0A000`, degenerate). Correlated `ORDER BY` keys have landed as `query.order_by_correlated`.
  - [ ] _follow-on:_ a **parenthesized-join FROM** (`FROM (a JOIN b ON …)`); a trailing **`ORDER BY`/`LIMIT` on a VALUES body**.
  - [ ] **Subqueries — remaining seams:** subqueries in an **`INSERT ... VALUES`** slot (blocked on VALUES holding a general expression); **row-valued** subqueries. _(size: S)_
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — precedence tree (INTERSECT binds tighter), full per-column type unification, NULL-safe multiset semantics, trailing ORDER BY by name/ordinal. → [grammar.md §25](spec/design/grammar.md)
  - [ ] _follow-on:_ parenthesized operands `(SELECT …) UNION …`; ORDER BY/LIMIT inside an operand; a set op in an `INSERT … SELECT` source.
- [x] **Common table expressions (`WITH`)** — named derived tables (PG hybrid inline/materialize), `WITH RECURSIVE`, data-modifying (writable) CTEs, nested `WITH`. → [cte.md](spec/design/cte.md), [recursive-cte.md](spec/design/recursive-cte.md), [writable-cte.md](spec/design/writable-cte.md)
  - [ ] _follow-on:_ recursive-CTE deferrals (`SEARCH`/`CYCLE`, a set-op / `FROM`-subquery recursive term, mutual recursion).
- [x] **Set-returning functions** — `generate_series` in FROM, a synthetic one-column relation, a `generated_row` cost unit. → [functions.md §10](spec/design/functions.md)
  - [x] _follow-on:_ the column-alias-list `AS g(c)` — explicit left-to-right output renaming for every fixed-shape table function (partial lists allowed; too many names → `42P10`). (`LATERAL` ✅ landed; `unnest(array)` ✅ landed — AF3.)
- [x] **`FOREIGN KEY` constraints** — column-/table-level `REFERENCES`, composite + self-reference, same-type pairing (`42804`), MATCH SIMPLE, enforced at four write sites (`23503`), plus recursive `ON DELETE`/`ON UPDATE` `CASCADE | SET NULL | SET DEFAULT` action closures (`format_version` 30's three-bit action fields). → [constraints.md §6](spec/design/constraints.md)
  - [x] **Child-index reverse match** — a parent DELETE/UPDATE's NO ACTION/RESTRICT search probes a usable child key (the child PK or a non-partial plain-column B-tree leading with the FK columns) and otherwise full-scans; both paths are metered, so `max_cost` bounds the O(parents × children) fallback. No index is auto-created (PG-matching). → [constraints.md §6.5/§6.11](spec/design/constraints.md)
  - [x] **Keyed referential actions** — generated `CASCADE`/`SET NULL`/`SET DEFAULT` statements gather their child rows through the same reverse path (one prefix probe per distinct parent tuple, else one child scan with a hash membership test) instead of a generated OR of tuple equalities, and `ON UPDATE CASCADE` maps old→new tuples instead of an N-arm `CASE`: O(parents × log children) indexed, O(children + parents) unindexed, single- and multi-column. → [constraints.md §6.6](spec/design/constraints.md)
  - [ ] _follow-on:_ `MATCH FULL`; FK type pairing relaxed to PG's comparable-types.
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — on-disk B-trees with equality/range and multi-column-prefix bounds, expression and partial keys, bounded mutation scans, and LIMIT streaming; `format_version` 5/26/27. → [indexes.md](spec/design/indexes.md)
  - [x] Safe `timestamptz` expression keys and partial predicates: compositional admission, named-zone version/checksum pins, dynamic-zone cache safety, and atomic transactional rebuilds (format 32). → [index-dependencies.md](spec/design/index-dependencies.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ a variable-width range/tail column (self-delimiting skip, not fixed width); ordered (`ASC`/`DESC`/`NULLS`) keys; `IF NOT EXISTS`; **partial-index relaxations** — a full partial-index scan without a leading access predicate, partial OR/IN / ORDER-BY-skip / INL bounds, an ON CONFLICT partial arbiter, a general predicate-implication prover (beyond the syntactic match). (`jsonb` and composite keys are tracked separately above.)
- [x] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* via a type-generic operator-class seam: the **`array_ops`** opclass (one entry per distinct non-NULL element, `format_version` 12's `index_kind` byte, a `gin_entry` cost unit) accelerating `@>`/`&&`/`= ANY(col)`/array `=` for SELECT + GIN-bounded UPDATE/DELETE, over the fixed-width key-encodable element types. → [gin.md](spec/design/gin.md)
  Bounded LIMIT streaming completes the posting gather, then stops candidate table lookups/filtering at the window.
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows) / `IN` over a scalar list; the **remaining** element types — the VARIABLE-width keyables (`text[]`, `bytea[]`, `decimal[]`) need GIN term framing; `float[]` and `interval[]` are now UNBLOCKED (fixed-width keys landed) but each is its own slice — plus composite-element arrays; multi-column GIN; correlated / array-column query operands; posting-list run compression; the **`jsonb_ops`** opclass and a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints** — a third index *kind* (`index_kind = 2`) whose payoff is PG exclusion constraints (`EXCLUDE USING gist (col WITH op)`, `23P01`); an operation-deterministic R-tree (a structural divergence — jed's own tree bytes), the `range_ops` + fixed-width scalar-`=` opclasses, multi-column GiST; `format_version` 21. → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ the `EXCLUDE … WHERE (predicate)` partial form; `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique index; **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape.
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <items>` evaluated after validation before any write; the PG-18 `old.`/`new.` row-version qualifiers and `WITH (OLD AS o, NEW AS n)` aliasing form have landed, including qualified-star expansion. → [grammar.md §32](spec/design/grammar.md)
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] }`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter; two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT`; `INSERT INTO t AS alias`; the partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed, but the conflict-path re-key is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [x] **`ALTER TABLE`** — catalog-only rename/default/nullability/constraint edits plus the
  `ADD`/`DROP COLUMN`, `ALTER COLUMN … TYPE … [USING]`, and `ADD`/`DROP PRIMARY KEY` rewrite slices
  have landed. → [alter.md](spec/design/alter.md)
  - [ ] _deferred:_ identity management (`ALTER COLUMN … ADD/DROP/SET GENERATED …`) — plausible once
    scheduled; the rest of PG's menu (OWNER/RLS/triggers/partitions/tablespaces/schemas/storage knobs) is
    deliberately out of scope, `0A000` (alter.md §6).
- [ ] **Temporary tables — slice 3: spill-to-disk** — the rest of the feature is landed: session-local
  temp relations with zero writes to the database file, each domain a per-session in-RAM
  `MemoryBlockStore` + pinned pager with within-session compaction and a page-based `54P03` budget;
  CREATE/DROP INDEX, serial/IDENTITY, composite columns; `allow_temp_ddl`. (The database-wide `SHARED`
  kind was removed in favor of in-memory attachments —
  [attached-databases.md §6](spec/design/attached-databases.md).) Remaining: spill a temp domain to
  disk — a temp-`BlockStore` swap + bounded pool (the blockstore flip already put temp on the seam).
  → [temp-tables.md](spec/design/temp-tables.md) _(size: M–L; deps: storage seam (done))_
  - [ ] _follow-on:_ `ON COMMIT DELETE ROWS`/`DROP`; `IF NOT EXISTS`; `CREATE TEMP TABLE … AS SELECT`; FKs among same-kind temp tables; temporary views. → [temp-tables.md §14](spec/design/temp-tables.md)

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
> (constant folding, CSE, short-circuit) are **not** cost-neutral here — they drop `operator_eval`
> charges — so each needs an explicit cost
> decision, not a silent apply. Every optimization is a vertical slice carrying a **NoREC relation**
> (the standing §7 obligation — the sweep does not discover new optimizations).

### Cost-based planner follow-ons

- [x] **Cost as a plan input (Path B)** — landed end-to-end: exact transactional row counts,
  relation-scoped prepared-plan validity, complete candidate inventory, whole-plan estimates and
  EXPLAIN columns, costed access/order pipelines, bounded deterministic N-way join search, and
  transactional v29 `ANALYZE` statistics. The canonical contracts are
  [estimator.md](spec/design/estimator.md), [statistics.md](spec/design/statistics.md),
  [planner.md](spec/design/planner.md), and [explain.md](spec/design/explain.md).
  - [x] _follow-on:_ cost-based `UPDATE`/`DELETE` access policy — the target scan takes the
    estimated-cheapest inventory candidate with the canonical tie order; the selected path's
    emission order is the phase-one visitation (error and `nextval`) order
    ([estimator.md §9.3](spec/design/estimator.md), `dml.costed_access`).
  - [ ] _follow-on:_ parameter-sensitive/custom prepared plans; planning remains pre-bind today.
  - [ ] _follow-on:_ statistics quality — pattern selectivity, extended/multi-column correlation,
    configurable targets, MCV-aware join skew, automatic analyze, and distribution facts for
    composite/array/json/jsonb.
  - [ ] _follow-on:_ broader physical search — costed outer/barrier shapes, bushy trees, parallel
    plan search, or adaptive runtime re-planning; each requires a bounded deterministic contract.

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
  - [x] _follow-on:_ push WHERE/ON conjuncts **into derived-table bodies** (`query.derived_pushdown`
    — the body is planned again with them, so they can bound its key), onto **CTE references, SRFs,
    and catalog relations** in a join, and **single-side ON conjuncts** (`query.on_pushdown` — either
    side of INNER, the NULL-extended side of LEFT/RIGHT; a pushed ON conjunct is also an access-path
    input for its relation). [planner.md §3.2–§3.3](spec/design/planner.md); NoREC `on_pushdown` and
    `derived_pushdown`.
  - [x] _follow-on:_ push into **grouped bodies** (a conjunct over bare-input grouping columns of a
    single grouping set moves below the grouping into the body's WHERE; ROLLUP/CUBE/several sets,
    expression keys, and aggregate outputs stay outside) and into **every set-operation arm** (all six
    operators, nested; each arm's column must be bare and of exactly the unified output type).
    [planner.md §3.2](spec/design/planner.md); NoREC `grouped_setop_pushdown`.
  - [ ] _follow-on:_ push into **inlined CTE bodies** (needs per-reference body specialization: the
    inline/materialize mode is decided after the referencing query is planned), **below a window**
    (a conjunct over every PARTITION BY column), and into a **widened set-operation arm** (would need
    the conjunct re-resolved at the arm's type).
  - [x] _follow-on:_ **contradiction proofs for UPDATE/DELETE** (`dml.where_contradiction`) — the
    SELECT proof over the target's resolved, unfolded WHERE; a proven mutation reads no target page
    or row and evaluates no WHERE/SET/CHECK/RETURNING item, while uncorrelated subqueries still fold
    and charge. [planner.md §3.1](spec/design/planner.md); NoREC `dml_contradiction`.
  - [ ] _follow-on:_ broader safe-conjunct gate (non-trapping casts/functions via a catalog
    `traps` classification) and parameter-time proofs.
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
  - [x] **Recover from serialization-only commit errors** — defer the storage commit guard until
    encoding succeeds; retain writer/coordinator usability across exclusive, shared, and attachment
    paths, with shared process coverage and fault-device checks for zero storage mutations.
  - [ ] **macOS measurements** — run the existing pure-Go `F_BARRIERFSYNC`/`F_FULLFSYNC` probe
    and the production validated-COW benchmarks on Mac hardware; Linux cannot establish Apple
    hardware durability or latency. Retain the existing full durable-completion guarantee.

- [x] **Shared multi-process file access** — ✅ **landed** ([locking.md](spec/design/locking.md)). The stable `protocol-v1` bundle, alone fast path, global writer, commit/meta gate, append-only shared commits, foreign-plan invalidation, independently coordinated file attachments, and Rust↔Go↔Node real-process matrix are implemented. The independent TypeScript engine uses the narrow first-party `impl/ts/native-lock` Node-API adapter; the full Rust wrapper remains a workload-dependent reach experiment. `locking = auto` is shared on capable local hosts. Pre-protocol binaries still require a one-time drain. → [locking.md](spec/design/locking.md), [benchmarks.md §7.3](spec/design/benchmarks.md)
  - [ ] **Node prebuilt distribution matrix** — ship no-install-script Node-API-8 artifacts for Linux glibc/musl x64+arm64, macOS x64+arm64, and Windows x64 with SHA-256/provenance and packaging tests. Source/dev builds already use `mise run ts:lock_build`; missing production artifacts fail closed `0A000`.
  - [ ] **Expanded shared-file fault/platform matrix** — extend the landed real-process handoff, timeout, pinned-reader, killed-writer, and attachment scenarios with deterministic body-sync/meta-publish fault hooks, a joiner arriving mid-compaction (the basic presence refusal + later join is `compact.process.toml`), symlink/hard-link process lanes, and every supported Windows/macOS packaging lane.
- [x] **File compaction / shrink (return space to the OS)** — the host-invoked `compact(name)` rewrites `main` or an attachment as the from-scratch image of its committed snapshot at `txid + 1`, streamed page by page into `<path>.jedtmp` and atomically renamed into place (in-memory: a fresh `MemoryBlockStore`). Never waits: a held writer gate, a pinned reader, or another process holding the file is `55006`; read-only `25006`; OPFS `0A000`. Rows, results, costs, and prepared statements survive; the shared process corpus covers presence refusal and a later join (`compact.process.toml`). → [api.md §2.6](spec/design/api.md), [storage.md](spec/design/storage.md)
  - [ ] _follow-on:_ **OPFS compaction** — write the image to a sibling OPFS file and swap it in once a crash-safe replace is designed for the browser host (`0A000` today). _(size: M)_
  - [ ] _follow-on:_ **in-place trailing-free truncation** — lower `page_count` and truncate when the top pages are all free (the SQLite `incremental_vacuum` flavor): no rewrite, but sequenced against the two-meta fallback and the watermark. _(size: M)_
  - [ ] _follow-on:_ compaction options — repack half-empty leaves (today nodes are kept as-is so costs are unchanged), change `page_size` during the rewrite, and stream the GiST R-tree pages instead of building one index's pages at a time. _(size: M)_
- [ ] **Attached (linked) databases — Slice 3: multi-file atomic write** — everything else is landed
  (all 3 cores): the design plus Slices 0 (retire `SHARED` temp), 1a (qualified names), 1b (in-memory
  `db.attach`/`detach`, N-root commit, cross-attachment joins, read-only `25006`, detach-in-use
  `55006`), 1b-3 (attachments inside the concurrency differential net), 1c (temp as scoped routing),
  and 2 (host-API *file* attach + single-database durable write, one-durable-writer `0A000` at
  commit). Load-bearing decisions are recorded in the doc: attach is host-API only, never SQL; the
  qualifier is a database, not a schema; no silent shadowing; the linkage is never persisted.
  Remaining: **multi-file atomic write** — 2PC via a super-journal, lifting the one-durable-writer
  rule. → [attached-databases.md](spec/design/attached-databases.md) _(size: L; deps: N-root commit
  (done); §9/§13)_
- [x] **Blocking sort and hash-operator spill** — `ORDER BY`, hash `JOIN`, `GROUP BY`/aggregate and `DISTINCT` use bounded row/state buffers under `work_mem` on native file hosts. Ordered disk hash partitions preserve JOIN candidate order, aggregate fold order, first occurrence and exact costs; direct scan feeds and cost prepasses stream. Aggregate DISTINCT and growing ordered-set/hypothetical collections spill, with bounded external ordering and descriptor counts. Shared forced-spill corpus checks run in CI; `mise run bench:spill` checks wide-input results/costs and measures RSS. → [spill.md](spec/design/spill.md)
  - [ ] **Remaining query-memory owners** — Q1 (row buffers, result collectors), Q2 (operator state), and Q3 (pending writes) have landed; a file-backed size cap (memory.md §8.7), remaining scalar kernels, and host scratch support for OPFS/WASI remain. `work_mem` is an operator threshold, not a whole-query memory ceiling. → [memory.md](spec/design/memory.md), [hosts.md](spec/design/hosts.md)
- [x] **Point-lookup, cold-read, and INSERT performance work** — the measured point-lookup,
  checksum/PAX/buffer-pool, concurrent-fault, and INSERT execution/tree slices have landed; final
  timings, allocation evidence, checksums, and decisions live in
  [benchmarks.md](spec/design/benchmarks.md), [pager.md](spec/design/pager.md), and
  [locking.md §9.3](spec/design/locking.md).
  - [ ] _follow-on:_ evaluate unique-dirty mutation for remove/rebalance separately; Slice 3 deliberately
    leaves DELETE on the pure copy-on-write path until its own allocation evidence justifies the extra
    mutable merge/split cases. Rust-only, no byte or cost change. _(size: M)_
- [x] **Large values — overflow pages + compression (TOAST-equivalent)** — large `text`/`bytea`/`decimal`/`json` pushed out-of-line onto overflow-page chains (`format_version` 3), optionally LZ4-compressed first via a deterministic hand-rolled block codec (no third-party dep — a library fails §8 byte-identity). → [large-values.md](spec/design/large-values.md), [lz4.md](spec/fileformat/lz4.md)
  - [ ] _follow-on:_ chain sharing on rewrite (let a rewritten record keep an unchanged value's existing chain — a byte-layout change, lands in all cores + incremental tests together).

---

## Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters + sessions
> + the CLI have landed; OPFS spill + the e2e-in-CI gap remain. Parallelizable with most feature work.

- [x] **Unify the create constructor** — the five overlapping DB constructors collapsed to two:
  `create(opts)` (fresh, either backing; `opts.path` absent → in-memory) and `open(path, opts)`
  (existing file); Go's exported `OpenDatabaseWithOptions`/`OpenOptions` closed the last open-surface
  divergence. Host-API only, byte-neutral. → [api.md §2.1.1](spec/design/api.md)
  - [ ] _follow-on:_ the anticipated create-time knobs as new `CreateOptions` fields
    (`max_storage_bytes` has landed as the storage limit, memory.md §8 / Q4a and Q4c; next a spill `temp_dir`, then a thread count);
    optionally sweep the async OPFS/browser host (`createOpfs`/`OpfsDatabase.create`) into the
    unified `create(opts)` shape.
- [ ] **Storage hosts** — the five-method `BlockStore` byte device, host catalog, and decoration layering (encryption codec above the seam, replication tee below) authored in [hosts.md](spec/design/hosts.md). **Landed:** the per-core `FileBlockStore`s, the Node `fs` host, and the **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity vs goldens, gated Playwright e2e). **Open:** OPFS disk-spill and running the real-browser e2e inside `mise run ci`. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [x] **jed-migrate — the schema-migration library** — tern-modeled, opt-in migration packages at
  `/migrate` (Go + Rust + TS; a NON-CORE consumer per language) over one shared contract +
  `testdata` corpus; one committed transaction per step (resumable); bundled by the CLI as
  `jed migrate`. → [/migrate/design.md](migrate/design.md)
  - [ ] _follow-on ([design.md §11](migrate/design.md)):_ the per-migration ledger table
    (`(sequence, name, checksum, applied_at)` — drift detection, out-of-order, truthful status); a
    `set-version` baseline (adopt an existing DB); an all-or-nothing whole-run transaction mode; an
    `OnStart` progress callback; a `renumber` collision helper.
- [ ] **Schema introspection — I3: `jed_sequences` + `jed_types`** — I0 (the `jed_` name reservation,
  `42939`), I1 (`jed_tables` + `jed_columns`), and I2 (`jed_indexes` + `jed_constraints`) are landed
  (all 3 cores): read-only computed relations resolved in every database's relation namespace, derived
  from the pinned catalog snapshot (no storage, no format bump), riding the SRF plan shape; per-table
  `SELECT`-gated (`42501`), write/DDL targets `42809`, one `generated_row` per row; caps
  `introspect.*`. SQL against the `jed_` relations is the whole surface (no host-API convenience);
  `information_schema`/`pg_catalog` are recorded non-goals. Remaining: **I3** (`jed_sequences` +
  `jed_types`); a `DEFAULT`-rendering column once a canonical expression-text form is pinned; the
  EXCLUDE operator list as a `jed_constraints` column addition.
  → [introspection.md](spec/design/introspection.md) _(size: M; deps: none)_
- [x] **Structured error fields** ([error-fields.md](spec/design/error-fields.md)) — ✅ LANDED (all 3 cores). `EngineError` gained four optional identifier fields modeled on pgx's `pgconn.PgError` — `ConstraintName`/`TableName`/`ColumnName`/`DataTypeName` (Rust `Option<String>`, Go `string`, TS optional) — so a host identifies *which* constraint fired without regexing the (non-contractual) message. Populated via **typed constructor helpers** that own message *and* fields together (no drift): 23505/23514/23503/23P01 → constraint+table; 23502 → column (+ table stamped at the DML boundary via `stampTable`/`.map_err`); 22003 routes through the `overflow(ty)` helper + 22001 varchar → data type (+ column). `Display`/`Error()` unchanged; per-core `error_fields` tests (corpus can't assert structured fields). **Hard-excluded:** pgx's `File`/`Line`/`Routine` (core source location differs across cores → would break §8 byte-identity). No format bump, no cost/determinism change.
  - [ ] _follow-on:_ `Detail` (the offending values, `Key (id)=(1) already exists` — the leading phase-2 field; revisits the no-DETAIL-line house style + needs value formatting through the deterministic text path); `Position` (1-based query offset for 42601/42703 — needs the parsers to thread byte positions); `Hint`; a `DatabaseName` analog for pgx's `SchemaName` (jed qualifies by database, not schema). All additive. → [error-fields.md §7](spec/design/error-fields.md)

---

## Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Several items are **ongoing obligations**
> that grow with each feature, not one-shot tasks.

- [ ] **Differential-testing harness** vs the PostgreSQL oracle (§7) — **PARTIAL.** The live-`db` oracle-import tool is built (`scripts/oracle_import.rb`; `mise run corpus:import` / `corpus:check`; the override ledger `spec/conformance/oracle_overrides.toml`). *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated** reference provisioning §12 — never auto-provision). SQLite is deliberately not an oracle; mining its sqllogictest corpus for query *shapes* (answers from PG) is the only oracle-adjacent use. _(size: M remaining)_
- [ ] **SQLancer-style metamorphic / generative testing** — **PARTIAL.** Built so far (`scripts/norec_gen.rb`; `mise run corpus:norec_sweep`, in `mise run ci`): the **NoREC** slice (pushdown vs non-optimizable rewrite must agree), the **TLP** slice (ternary-logic partitioning — now including **aggregate-TLP** for `COUNT`/`SUM`/`MIN`/`MAX`, both ungrouped via scalar-subquery combination — `SUM` by `COALESCE` (grammar.md §51), `MIN`/`MAX` by `LEAST`/`GREATEST` (grammar.md §52) — and per-`GROUP BY`-group via a UNION-ALL derived-table super-aggregate), and an automatic **reducer** (`scripts/reduce.rb`, ddmin). *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator), `AVG` aggregate-TLP (deferred — its by-construction expected is an exact-`decimal` `SUM`/`COUNT` division whose scale/rounding the generator would have to replicate), and broader NoREC relations. _(size: M remaining)_
- [x] **Oracle decoupling: stop being calibrated to one PostgreSQL** — **DONE (O1 + O2 + O3).** The corpus's expected output is filled from a live PG server, whose *cluster configuration* — not PostgreSQL itself — changed the answers. **O1:** the oracle's configuration is declared data (`spec/conformance/oracle_profile.toml`) — `[cluster]` facts asserted once per process at connect (abort on mismatch, warn on tzdata drift), `[session]` GUCs applied as `SET LOCAL` in every probe's preamble, a static completeness check in `mise run verify`, and `mise run oracle:setup` / `reset` / `status` / `check`. **O2:** the oracle moved onto a dedicated `jed_oracle` database using PG 17+'s platform-independent **`builtin` `C.UTF-8`** provider, whose code-point ordering *is* jed's single defined `C` collation, so text ordering agrees by construction; four collation entries retired from the override ledger (192 → 188) and a fifth amended (it was really a scan-order entry). **O3:** the RQG firehose's `COLLATE "C"` chokepoint is gone (`scripts/lib/rqg/expr.rb` + 5 `ctx.use(:collate)` sites), and **set-op `ORDER BY` coverage is restored** — `shapes/setop.rb` had omitted it *because* a set-op ORDER BY cannot carry an explicit collation. A 1500-case sweep agreed 1500/1500 with zero divergence, which is the evidence the oracle is locale-independent; `suites/rqg/*.test` regenerated (25 → 30 blocks per shape, zero `COLLATE`, 15 ordered set-op cases). _(size: done)_
- [ ] **Corpus growth** (ongoing) — keep adding `.test` coverage as each feature lands. Two **standing obligations** (conformance.md §5/§8): (a) on the PG-comparable surface, run `mise run corpus:check` and register any intentional divergence in the override ledger; (b) **when you add a query optimization or a new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does not discover new optimizations.
- [ ] **Benchmark backfill** (ongoing) — grow `bench/corpus` beyond the v1 set (benchmarks.md §11): GROUP BY aggregate, miss-heavy point lookups, text/large-value-heavy rows, concurrent-reader cross-engine (PG/SQLite) + truly larger-than-pool eviction/thrashing variants (the jed-only resident and cold-population `concurrent_read_pk*_r{1,4}` pairs landed — benchmarks.md §8.1), cold-open time, durable-commit batch-size sweep. **Standing obligation** (§10): a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected benchmarks before/after. _(size: M, ongoing)_

---

## Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, [cores.md](spec/design/cores.md)).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). Any native core still passes the full conformance contract; a wrap inherits it.

- [x] **Ruby gem** — wrap the safe Rust core, shipped as a gem (conforms by construction; a distribution artifact, not an independent conformance voice). **Landed:** Slice 1 (the `cdylib` + a `fiddle`-loaded pure-Ruby gem), Slice 2 (`$N` bind params), Slice 3 (richer typed values — `BigDecimal`/`Date`/`Time`, AR-style), Slice 4 (host-loaded `JUCD`/`JTZ` bundles), and the binding-overhead benchmark (`bench/ruby`). → [ruby.md](spec/design/ruby.md), [cores.md §6](spec/design/cores.md)
  - [ ] _follow-on (each its own slice):_ a **gem prepared-statement API** (isolates the pure FFI tax from per-call parse); **`interval`/`uuid`/`bytea`** typed coercion (left as String); **distributable packaging** — a `gem install`-able native gem via `rb-sys` + precompiled platform gems (or `magnus`), replacing the in-repo `mise run ruby:build` (a wrapper-module dep — needs §14 confirmation); an optional **Ruby conformance runner**. In-process Ruby host functions ride on the vectorized/batched host-function API below. _(size: L wrap)_
- [ ] **Native Node/Rust reach package experiment** — the benchmark-capable prototype is built (`impl/node`, `bench-node-rust.ts`), exact-pinned, and checksum-verified against all 53 lanes. It is not production-complete: add the full typed value/error/session API, async/worker-thread story, host-function policy, prebuilt/provenance matrix, packaging tests, and a package-name/product decision before presenting it as an alternative to native TS. Do not count it as a conformance core—it echoes Rust. Current performance decision is workload-dependent; see [cores.md §5.2](spec/design/cores.md) and [benchmarks.md §7.3](spec/design/benchmarks.md). _(size: L wrap)_
- [ ] **C#** core — **lean native** (value-type generics, `Span<T>`, NativeAOT → near-Rust speed *and* a clean pure-managed NuGet package; in-process host functions). Strongest native candidate. _(size: XL native / L wrap; §2)_
- [ ] **Swift** core — **lean wrap** (UniFFI + XCFramework over the safe Rust core: Rust speed, well-trodden Apple packaging, untrusted-query safety preserved §13). Native only if hot-path per-row host functions make the FFI upcall tax dominant. _(size: L wrap / XL native; §2/§13)_
- [ ] **Java** core — **conflicted**: wrap for performance (pre-Valhalla boxing + JIT warmup hurt a native core), native for clean pure-JAR packaging + in-process host functions (no JNI/upcall tax). Decide at scheduling time; Valhalla shifts it toward native. _(size: XL native / L wrap; §2)_
- [x] **Runtime function registry** — built-in named scalar + aggregate resolution is data-driven over the generated catalog tables (one `(name, arg_families)` lookup). → [extensibility.md §5](spec/design/extensibility.md)
  - [ ] _follow-on:_ built-in type-vtable dogfood (Fork A).
- [x] **Host scalar functions over existing types** (extensibility.md §14 step 3, all 3 cores) — a host builds an `ExtensionRegistry` (scalar functions over the built-in types), passes it in `CreateOptions`/`OpenOptions`, and the engine freezes it for the handle + resolves/evaluates host calls through a registry dispatch arm beside the built-in one (reached by id). Exact-signature overloading (built-ins win an exact collision, `42723` on a duplicate registration); ephemeral (no persisted use / no DDL); strict; declared static **cost** charged per call + guarded; wrong-typed kernel result caught (`22000`). → [extensibility.md §4.2 / §5.1](spec/design/extensibility.md)
  - [ ] _follow-on:_ **vectorized/batched host-function ABI** — batch-of-one only for now; the column-in→column-out ABI is the single decision that keeps *wrapping* viable (amortizes the per-row FFI upcall for Swift/Java/C#). _(size: M; §2, cross-cutting)_
  - [ ] _follow-on:_ non-strict host functions, host functions over container args, and runtime **enforcement** of the recorded `cross_core` declaration (taint/containment of non-cross-core results — no runtime taint mechanism exists yet, matching how `float` is handled at the spec layer). _(`immutable` is now enforced for index-backing, and an immutable host function may back an index — below.)_
- [x] **Host functions in persisted indexes** (extensibility.md §14 step 4, all 3 cores, `format_version` 31) — **Model B (registry-only, no SQL DDL)**: a `HostFunction` gains `component_id` + `semantic_version`; an `immutable` + versioned host function may back an expression / partial index (`CREATE INDEX ON t (geo_hash(a))`). The index catalog persists a per-index host-function **dependency list** (`index_flags` bit2 `has_host_deps` + the `(name, arg types, result, component_id, semantic_version)` tuples); the per-statement re-resolution re-checks it — a **read** whose candidate index no longer matches skips it (correct heap scan), a **write** is refused (`XX002` component/version mismatch, `42883` missing function), never a silent stale-key read. Also **fixed a latent bug**: a host function (even `volatile`) previously leaked into an index expression (the immutability gate was purely syntactic) — now `42P17`. → [extensibility.md §7 / §8.1 / §11](spec/design/extensibility.md)
  - [ ] _follow-on:_ host functions in `DEFAULT` / `CHECK` / generated columns (lower-risk — no stored keys); the full [compatibility.md](spec/design/compatibility.md) per-object graded reopen manifest (step 4 landed the index-only form); `REINDEX` to rebuild a stale/unusable index.
- [x] **Host-defined functions must contribute to the cost system** — **design (a) (declared static weight) landed** for the ephemeral slice above: registration carries a non-negative `cost`, charged once per call and guarded against `max_cost` (aborts `54P01` before the kernel runs). The two contracts hold (the untrusted-query bound §13 + cross-core cost identity §8). _Remaining:_ the richer cost designs (b) declared cost-as-a-function-of-arguments and (c) a metering callback for chunk-boundary mid-call abort, and the "no cost ⇒ `max_cost = 0` only" admission (this slice makes `cost` required, always admissible). → cost.md §6. _(size: S; §2/§13)_

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

## Resource admission follow-through

- [x] **JSON nesting-depth limit.** A fixed `MAX_JSON_DEPTH = 256` bounds every `json`/`jsonb`
  document where it is produced: the text parser and every nesting constructor (builders,
  aggregates, `to_json[b]`/`array_to_json`, `||`, `jsonb_set`/`jsonb_insert`,
  `jsonb_path_query_array`, the `JSON_QUERY` wrapper) raise `54001`, and the on-disk `jsonb`
  decoder raises `XX001`. The jsonpath compiler bounds its own nesting at the same 256,
  `JSON_TABLE NESTED PATH` counts toward `MAX_EXPR_DEPTH`, and `array_in` stops at the 7th brace.
  This closes the native-stack crash from untrusted deeply nested JSON. See
  [json.md §6.4](spec/design/json.md) and `resource/json_depth_limit.test`.
  - [ ] _follow-on:_ when jsonpath `.**` lands, keep its recursive descent bounded by document
    depth; when a storable `jsonpath` column lands, its decoder must re-run the compiler's
    nesting gate (`XX001` on a crafted body).
- [x] Proportional UTF-8 work charging and pre-allocation byte guards for repeat/padding;
  guarded decimal transcendental entry/series/range-reduction/power steps; finite 64 MiB
  cumulative scalar allocation allowance (`max_scalar_bytes`, `54P04`), shared across
  internal meters and frozen cursors. See [cost.md §8](spec/design/cost.md) and
  [memory.md](spec/design/memory.md).
- [x] **Q1 — rows.** Live, opt-in `max_query_memory_bytes` account (`54P05`, unlimited by
  default) over row buffers and engine result collectors, in deterministic logical bytes
  from the shared schedule (`spec/cost/schedule.toml` `[memory]`, vectors in
  `spec/cost/memory_sizes.toml`); exact thresholds pinned by `resource/query_memory*.test`;
  `mise run conformance:query_memory` runs the whole corpus with accounting active. See
  [memory.md](spec/design/memory.md) §2–§5.
- [x] **Q2 — operator state.** Hash-join, group, DISTINCT, set-operation and recursive-CTE
  tables (`ENTRY` 32 + key), retained aggregate inputs, window partitions, sort state,
  columnar lanes, and spill structures' resident elements; spill residency is measured on
  the same schedule so every core spills at the same row and releases on spill. Thresholds
  pinned by `resource/query_memory_state.test`; `mise run conformance:query_memory` compares
  every record's peak across cores in memory, disk, and forced-spill modes. See
  [memory.md](spec/design/memory.md) §6.
  - [ ] _follow-on:_ the transient owners §6.8 leaves out (collation sort-key decorations,
    window-frame operand caches, merge-heap heads). (The budget-aware spill trigger has landed:
    a spill-capable structure spills when the account rejects it — memory.md §6.6.)
- [x] **Q3 — pending writes.** Every record version a transaction stages into a table or index
  reserves its stored bytes (on-disk record + out-of-line payload) against
  `max_query_memory_bytes` until commit/rollback; each statement's account opens holding them,
  and they must fit after every statement. Thresholds pinned by
  `resource/query_memory_pending.test`. See [memory.md](spec/design/memory.md) §7.
  - [ ] _follow-on:_ a within-statement check (today a statement's own writes are checked once it
    completes, like `temp_buffers`), and a net-residency measure (today a rewrite charges again).
- [x] **Q4a — committed in-memory storage.** A database-owned `max_storage_bytes` (create/attach
  option + handle setter, unlimited by default; the file form is Q4c below) over
  `page_count × page_size`, checked at commit before any write; a commit that raises the
  high-water past the limit fails `54P06` after one forced compaction, with a repair exemption for
  pure deletes and drops and a multi-root precheck. The compaction trigger is shared data
  (`[reclamation]` in `spec/cost/schedule.toml`); the `storage_bytes` gauge and a per-record
  cross-core storage comparison in `mise run conformance:query_memory` back it. Thresholds pinned by
  `resource/storage_memory.test` and `concurrency/storage_watermark.test`. Page caches stay
  evict-only under `cache_bytes`. See [memory.md](spec/design/memory.md) §8.
  - [x] _Q4b:_ the resource-limits web page documents the §8.6 cache rules (evict-only,
    live-reference overshoot, the interior skeleton and GiST tree outside `cache_bytes`) and the
    B/2–B sizing note.
  - [x] _Q4c:_ the file form — `max_storage_bytes` on a file-backed database or file attachment
    limits its **live** pages (reachable from the catalog root), an exact count persisted at meta
    offset 56 (`format_version` 34) and advanced by an exact per-commit delta, so the trip point is
    independent of the free list, the watermark, and co-resident append-only allocation. Create /
    open / attach options and the setter; no forced compaction. Pinned by
    `resource/storage_file.test` (disk) and `process/storage_limit.process.toml` (alone vs
    co-resident); every core recounts each commit by reachability under the disk corpus.
    See [memory.md](spec/design/memory.md) §8.7.
  - [ ] _follow-on:_ bounding the interior skeleton and resident GiST R-tree once they page.
- [x] **Output-constructing kernels** ([cost.md §8.1](spec/design/cost.md)). Growth kernels
  (array/JSON construction, concatenation, escaping renders) charge `scalar_byte × payload(result)`
  to cost after construction, which closes the geometric-growth hole (a recursive CTE doubling a
  value for a few units). Amplifiers (`replace`, `regexp_replace`, `array_replace`,
  `jsonb_pretty`) size their output first, then charge it and reserve it against the scalar
  allowance. Public docs must still distinguish the implemented budgets from a full heap limit.
  - [x] _S1:_ `ARRAY[…]`, array `||`/append/prepend/cat; `replace`, `regexp_replace`,
    `array_replace` (`resource/scalar_output.test`).
  - [x] _S2:_ JSON construction (jsonb `||`, `jsonb_set`/`insert`, the build/ctor functions,
    `to_json[b]`, `array_to_json`, `json_scalar`/`json_serialize`, `jsonb_path_query_array`, the
    `JSON_QUERY` wrapper) and `jsonb_pretty` (`resource/scalar_output_json.test`).
  - [x] _S3:_ escaping renders: `encode`, `quote_*`, array → text, and jsonb → text/json
    (`resource/scalar_output_render.test`). Casts to text from `bytea`, composites, and ranges join
    when those casts land.
  - [x] _follow-on:_ jsonpath metering ([jsonpath.md §7](spec/design/jsonpath.md)): `jsonpath_compile`
    per compiled byte and `jsonpath_step` per step application, emitted item, member examined, and
    comparison pair (`resource/jsonpath_cost.test`). The Rust evaluator now borrows items instead of
    deep-copying them.
