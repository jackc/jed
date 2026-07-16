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
  - [ ] _follow-on:_ per-node actual cost attribution under `ANALYZE`; a full expression printer for
    the residual filter / projections (currently a `conjuncts=N` count) + exact float-literal bound
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
  - [ ] _follow-on:_ `to_char`/`to_timestamp`, `age`, `EXTRACT(julian …)`; a separate `time` type; **text⇄`timestamp`/`timestamptz` casts** + **session-zone rendering** of `timestamptz`; `timestamp(p)` precision typmods ([timezones.md §9](spec/design/timezones.md)). _(`date_part` (float8) has landed — the date-functions follow-on below.)_
- [x] **`date`** — calendar date (i32 days), strict ISO literals + BC + `±infinity`, date PK (type code 16); date arithmetic (`date ± int`, `date − date`, `date ± interval`). A strict island — no implicit compare to timestamp. → [date.md](spec/design/date.md)
  - [x] _follow-on (all landed):_ the runtime text→date cast (STABLE, un-indexable `42P17`); the clock-relative literals (`today`/`now`/`tomorrow`/`yesterday` as a STABLE never-folded node, `epoch` as a `parse_date` constant); and the date functions `make_date` / `date_part` (float8, the EXTRACT twin) / `current_date` (the bare keyword). → [date.md §6](spec/design/date.md)
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
- [x] **Composite `PRIMARY KEY`** — table-level `PRIMARY KEY (a, b, …)`, key bytes = members' concatenated encodings. → [constraints.md §3](spec/design/constraints.md)
  - [x] **Composite point/prefix pushdown** — maximal equality prefix plus an optional next-member range, shared by SELECT, UPDATE/DELETE, correlated scans, join-relation bounds, and INL; cap `query.composite_pk_pushdown`. → [cost.md §3](spec/design/cost.md)
- [x] **`FOREIGN KEY` constraints** — column-/table-level `REFERENCES`, composite + self-reference, same-type pairing (`42804`), MATCH SIMPLE, enforced at four write sites (`23503`), `format_version` 11. → [constraints.md §6](spec/design/constraints.md)
  - [ ] _follow-on:_ the referential **actions** `ON DELETE/UPDATE CASCADE | SET NULL | SET DEFAULT` (parse but `0A000` today); `MATCH FULL`; a **backing index** on the child FK columns (the parent-side check full-scans children today); FK type pairing relaxed to PG's comparable-types.
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — non-unique on-disk B-trees, maintained in the two-phase pass; the planner index-bounds a base scan on an access predicate; `format_version` 5. → [indexes.md](spec/design/indexes.md)
  - [x] **Index ranges + multi-column prefix bounds** — the index access predicate is a maximal equality prefix on the leading key columns plus an optional range on the next; caps `query.index_range` / `query.index_prefix`. → [indexes.md §5.1](spec/design/indexes.md)
  - [x] **Expression index keys** — `lower(email)` / `(a + b)` key elements, plain or `UNIQUE`, validated immutable, stored as canonical text (`format_version` 26), matched structurally by the planner; cap `query.index_expr`. → [indexes.md §1/§2/§6](spec/design/indexes.md)
  - [x] **Partial indexes** — `CREATE [UNIQUE] INDEX … WHERE predicate` (`format_version` 27); only predicate-TRUE rows are indexed/constrained; planner use requires a structurally-equal WHERE conjunct (syntactic implication); caps `ddl.index_partial` / `query.index_partial`. → [indexes.md §9](spec/design/indexes.md)
  - [x] **Index scans for `UPDATE`/`DELETE`** — the ordinary equality-prefix/trailing-range access predicate (including eligible expression/partial indexes) and the secondary-index OR/IN point set gather keyed old rows before the two-phase write; cap `query.index_mutation`. → [indexes.md §5.1](spec/design/indexes.md)
  - [x] **Bounded LIMIT streaming** — contiguous/index interval sets stream until OFFSET+LIMIT survivors; ORDER BY combines only with the same ordered index; cap `query.bounded_limit_streaming`.
  - [ ] _follow-on (each its own slice + NoREC obligation):_ a variable-width range/tail column (self-delimiting skip, not fixed width); ordered (`ASC`/`DESC`/`NULLS`) keys; `IF NOT EXISTS`; **partial-index relaxations** — a full partial-index scan without a leading access predicate, partial OR/IN / ORDER-BY-skip / INL bounds, an ON CONFLICT partial arbiter, a general predicate-implication prover (beyond the syntactic match), and lifting the conservative timestamptz-predicate `42P17`. (All scalar key types are now encodable; only the recursive `composite` container stays a `0A000` key.)
- [x] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* via a type-generic operator-class seam: the **`array_ops`** opclass (one entry per distinct non-NULL element, `format_version` 12's `index_kind` byte, a `gin_entry` cost unit) accelerating `@>`/`&&`/`= ANY(col)`/array `=` for SELECT + GIN-bounded UPDATE/DELETE, over the fixed-width key-encodable element types. → [gin.md](spec/design/gin.md)
  - [x] **Bounded LIMIT streaming** — posting gather remains complete; candidate table lookups/filtering stop at the window; cap `query.bounded_limit_streaming`.
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows) / `IN` over a scalar list; the **remaining** element types — the VARIABLE-width keyables (`text[]`, `bytea[]`, `decimal[]`) need GIN term framing; `float[]` and `interval[]` are now UNBLOCKED (fixed-width keys landed) but each is its own slice — plus composite-element arrays; multi-column GIN; correlated / array-column query operands; posting-list run compression; the **`jsonb_ops`** opclass and a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints** — a third index *kind* (`index_kind = 2`) whose payoff is PG exclusion constraints (`EXCLUDE USING gist (col WITH op)`, `23P01`); an operation-deterministic R-tree (a structural divergence — jed's own tree bytes), the `range_ops` + fixed-width scalar-`=` opclasses, multi-column GiST; `format_version` 21. → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ the `EXCLUDE … WHERE (predicate)` partial form; `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique index; **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape.
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <items>` evaluated after validation before any write; the PG-18 `old.`/`new.` row-version qualifiers landed. → [grammar.md §32](spec/design/grammar.md)
  - [ ] _follow-on:_ the `WITH (OLD AS o, NEW AS n)` aliasing form; `old.*`/`new.*`.
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] }`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter; two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT`; `INSERT INTO t AS alias`; the partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed, but the conflict-path re-key is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [x] **`ALTER TABLE`** — the last major DDL gap (a created table's shape is currently frozen).
  Designed spec-first in [alter.md](spec/design/alter.md); sliced lowest-risk → highest. Two mechanical
  facts drive it (alter.md §0): columns are identified by **dense 0-based ordinal** referenced
  positionally everywhere (PK/index/FK/EXCLUDE), and `CHECK`/`DEFAULT`/index/predicate expressions are
  stored as **re-resolved text** — so a rename must rewrite that text and a drop must renumber ordinals.
  This splits into catalog-only edits (no `format_version` bump) and table rewrites (rebuild the B-tree,
  but still emit an ordinary current-format table — also no bump). _(size: L overall; each slice below is
  its own vertical slice + oracle/NoREC obligation)_
  - [x] _slice 1:_ grammar (`alter_table` production) + the multi-action all-or-nothing frame +
    `RENAME {TO | COLUMN | CONSTRAINT}` + `ALTER COLUMN SET/DROP DEFAULT` + `SET/DROP NOT NULL`
    (catalog-only; `RENAME COLUMN` rewrites this table's stored expression text — alter.md §2.2).
    Later-slice ADD/DROP COLUMN and TYPE forms are recognized as `0A000`; PK-constraint rename is `42704`
    because jed has no named PK catalog object (alter.md §2.3/§7). _(size: M)_
  - [x] _slice 2:_ `ADD` / `DROP CONSTRAINT` (`CHECK`/`UNIQUE`/`FOREIGN KEY`/`EXCLUDE`) with the
    validating end-state scan — **retires** the `ALTER TABLE … ADD/DROP CONSTRAINT` follow-ons noted under
    the FOREIGN KEY and GiST/EXCLUDE items above (alter.md §2.6/§2.7). _(size: M)_
  - [x] _slice 3:_ `ADD COLUMN [constraints]` — the first rewrite; per-row `DEFAULT` evaluation through
    the entropy/clock seam (alter.md §3.1). _(size: L)_
  - [x] _slice 4:_ `DROP COLUMN` — the ordinal renumber + `RESTRICT`/`CASCADE` dependency handling
    (non-PK columns), including PG-matched automatic removal of an FK owned by a dropped local column;
    ledgered PG divergence: jed physically removes, no `attisdropped` tombstone
    (alter.md §3.2). _(size: L)_
  - [x] _slice 5:_ `ALTER COLUMN … TYPE … [USING]` + `ADD`/`DROP PRIMARY KEY` — the re-encode/re-key
    rewrites (the PK forms reuse the existing UPDATE-of-PK re-keying path — alter.md §3.3/§3.4). _(size: L)_
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
> corpus-assertable. Mutations and hard-fenced join shapes retain their documented fixed policies.
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
  - [ ] _follow-on:_ cost-based `UPDATE`/`DELETE` access policy, with mutation visitation/error order
    decided and re-pinned explicitly.
  - [ ] _follow-on:_ parameter-sensitive/custom prepared plans; planning remains pre-bind today.
  - [ ] _follow-on:_ statistics quality — pattern selectivity, extended/multi-column correlation,
    configurable targets, MCV-aware join skew, automatic analyze, and distribution facts for
    composite/array/json/jsonb.
  - [ ] _follow-on:_ broader physical search — costed outer/barrier shapes, bushy trees, parallel
    plan search, or adaptive runtime re-planning; each requires a bounded deterministic contract.

### Planner infrastructure

- [ ] **Predicate pushdown + simplification** — push WHERE conjuncts into derived tables / CTEs /
  through joins to the earliest relation, and detect contradictions (`x > 5 AND x < 3` → a provably
  empty scan). **Caveat:** plan-time **constant folding** / CSE removes `operator_eval` charges and so
  changes the observable cost — each such rewrite needs an explicit cost decision (the framing above),
  not a silent apply. _(size: M–L; ×3 cores; +NoREC)_

---

## Storage maturation (§9)

> The path to a **larger-than-RAM file that does not fall over** (CLAUDE.md §9): no
> full-residency assumption above the storage seam.

- [ ] **Shared multi-process file access** — ⏳ **revised design complete, not built** ([locking.md](spec/design/locking.md)). Today separate opens remain undefined corruption. The implementation is now shared-first: a stable, version-marked `<path>.lock/` coordination bundle carries crash-clean whole-file OS locks for presence, arrival, transition, global writer, and the short commit-publish gate. `locking = auto` defaults to shared on capable local file hosts; an OS-backed presence-EX lease leaves the common one-process **foreground path unchanged** (zero lock syscalls / meta preads per transaction), while a background probe opens a join window when another process arrives. Co-resident writes serialize globally, refresh meta before snapshot, and commit append-only (`free_list_head = 0`); reuse/reclamation/truncation/compaction require proven aloneness + the in-process watermark. No database-format bump and no persisted `catalog_gen` are needed for correctness. The first rollout must drain pre-protocol binaries, which cannot honor advisory locks. Required verification includes real Rust↔Go↔Node processes, kill-point recovery, marker/alias/hard-link behavior, and a fast-path no-regression benchmark. → [locking.md](spec/design/locking.md) _(size: XL; all cores in lockstep)_
  - [ ] **Node OS-lock adapter + delivery decision** — Node has no standard `flock`/`LockFileEx`; corruption-safe shared mode requires native code. The full-Rust alternative was built first as experimental `impl/node` with exact-pinned `napi 3.10.5`, `napi-derive 3.5.10`, and build-only `napi-build 2.3.2`, plus a complete 53-lane benchmark driver. All 159 pure-TS/wrapper/native-Rust results agreed on checksums. The wrapper won 49 single-process lanes by 1.38× geometric mean and concurrent lanes by 3.76×, but cheap lanes were tied, pure TS won writes by 2.04×, and Node calls cost 2.01× over native Rust. Current proposal therefore remains the smaller first-party addon exposing only nonblocking shared/exclusive try-lock + unlock: the alone lease makes zero foreground addon calls and preserves the existing TS single-process path. Keep the full wrapper as a reach experiment; decide separately whether it becomes an optional/primary package after its full API and packaging exist. Production prebuilt Node-API-8 artifacts target Linux glibc/musl x64+arm64, macOS x64+arm64, and Windows x64, with no install script/download; the bounded CLAUDE.md §14 exception and artifact matrix still need an explicit ship decision. Otherwise Node shared mode fails closed `0A000` (no PID/mtime stale-lock fallback). → [locking.md §8](spec/design/locking.md), [benchmarks.md §7.3](spec/design/benchmarks.md)
- [ ] **File compaction / shrink (return space to the OS)** — ⏳ **approach decided, not built.** The free-list recycles dead space for jed but `page_count` is a monotonic high-water, so the file is grow-only. Decided mechanism: a **host-invoked compaction** that re-serializes the committed snapshot through the from-scratch `to_image` serializer into a fresh file + atomic swap (the `create` temp-file + fsync + rename recipe), reclaiming all dead space + defragmenting (the SQLite `VACUUM` / PG `VACUUM FULL` flavor) crash-safely. Explicit / host-invoked; under the shared protocol it requires presence-EX proof of cross-process aloneness **and** a drained in-process reader watermark. The stable lock bundle remains held across close/rename/reopen, so the replacement never escapes coordination. A lighter in-place trailing-free truncation stays open as a cheaper partial complement. → [storage.md §6](spec/design/storage.md) _(size: M–L; deps: page reclamation (done), shared coordinator; §9)_
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
- [ ] **Streaming + spill-to-disk operators** — bound blocking operators (`ORDER BY`, hash `JOIN`, `GROUP BY`/aggregate, `DISTINCT`) by a memory budget and **spill to disk** when exceeded, so a query over larger-than-RAM data never materializes its whole input/output in memory. **Landed:** the **external merge sort for `ORDER BY`** (a `Sorter` bounded by `work_mem`, spills sorted runs + k-way merges, byte-for-byte identical to the in-memory sort). → [spill.md](spec/design/spill.md) _(size: XL; deps: paged storage; §9/§13)_
  - [ ] **Spilling hash aggregate / `DISTINCT` / hash JOIN** — the remaining blocking operators (spill.md §7). Each needs a *different* algorithm: a partitioned (grace) hash that preserves first-occurrence order for aggregate/DISTINCT, and grace-hash partitioning to bound the existing in-memory hash JOIN's build side without changing its stable probe/bucket order. _(size: L–XL each)_
- [ ] **Bench-driven perf follow-ons** — the measured gaps remaining after the `perf-point-lookup` work (which took `point_lookup_pk` past same-language PG clients in all 3 cores):
  - [x] **Prepared point lookup** — split ramp from fully-hot measurement and added p90/p99; retained
    packed leaf keys as block spans with lazy weights; fused complete-PK lookup into one counted
    descent/reconstruction; cached bind/result metadata and thinned frozen read contexts. Final
    results, checksums, and allocation evidence are recorded in
    [benchmarks.md](spec/design/benchmarks.md). No format/cost/result change.
  - [x] **Cold page-checksum fast paths** — preserved the format-v29 CRC-32/IEEE bytes and corruption
    timing while replacing the bit-at-a-time loops with Go `hash/crc32`, Node `node:zlib.crc32`, and
    safe slicing-by-8 in Rust/browser TypeScript. Ramp p90 fell from 51.2/34.8/62.5 µs to
    5.5/7.6/11.9 µs for Go/Rust/TypeScript; hot performance and both answer checksums were retained.
    No format/cost/result change. → [benchmarks.md](spec/design/benchmarks.md)
  - [x] **Cold PAX directory views** — retained fully validated key/value end-offset directories as
    page-backed byte ranges/offsets instead of per-leaf integer arrays in all three cores. Removes
    `1 + V` allocations for a leaf with `V` variable-width columns without changing file bytes,
    fault-time corruption detection, results, or cost. → [packed-leaf.md](spec/design/packed-leaf.md)
  - [x] **Bounded buffer-pool index reservation** — Rust and Go initialize the page-id map with a
    bounded `min(cache_leaves, 8192)` hint, covering the measured 6,900-leaf cold population without
    eagerly sizing to arbitrarily large caller cache budgets. Focused population time and allocation
    evidence is recorded in [benchmarks.md](spec/design/benchmarks.md); results, cost, and bytes are
    unchanged. → [pager.md](spec/design/pager.md)
  - [x] **Rust positioned page reads** — the file host uses safe standard-library positioned reads on
    Unix/Windows and retains a serialized `seek` + `read_exact` fallback elsewhere. The focused 8 KiB
    read probe improved 10.4% with allocations unchanged; the end-to-end cold ramp stayed inside
    benchmark noise with the same checksum. → [benchmarks.md](spec/design/benchmarks.md),
    [hosts.md](spec/design/hosts.md)
  - [x] **Concurrent cold-fault single flight** — Rust and Go hold the pool mutex only for
    hit/flight/CLOCK bookkeeping, share same-page loads, and run different-page checksum/PAX decode
    concurrently; pager locking still serializes physical I/O against commit. In-flight invalidation
    detaches old loads and prevents stale page-id reinsertion. Focused synchronization + reader/writer
    race tests and the shared cold r1/r4 benchmark are recorded in [pager.md](spec/design/pager.md) and
    [benchmarks.md](spec/design/benchmarks.md). TypeScript remains single-threaded.
  - [ ] **Rust CoW insert deep-clone** — `node_insert` rebuilds a path node with `Vec::clone`, deep-copying every key (`Vec<Vec<u8>>`) + row where Go's `[][]byte` copy is pointer-shallow (why `insert_rollback` is rust 21.6ms vs go 10.3ms). Fix: share entry storage (`Arc<[u8]>` keys / `Arc`-shared rows). Rust-only, no byte or cost change. _(size: M)_
  - [ ] **Full-scan materialization** — `full_scan_agg` clones every row into a buffer before aggregating (143–281ms vs PG ~13ms). Streaming aggregation over the scan visitor is the contained first step; the full fix is the spill item above. _(size: M–L)_
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
  - [ ] _follow-on:_ the anticipated create-time knobs as new `CreateOptions` fields (`memory_limit`
    first — the in-memory twin of `cache_bytes`; then a spill `temp_dir`, then a thread count);
    optionally sweep the async OPFS/browser host (`createOpfs`/`OpfsDatabase.create`) into the
    unified `create(opts)` shape.
- [ ] **Storage hosts** — the five-method `BlockStore` byte device, host catalog, and decoration layering (encryption codec above the seam, replication tee below) authored in [hosts.md](spec/design/hosts.md). **Landed:** the Node `fs` host, the `FileBlockStore` extraction, and the **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity vs goldens, gated Playwright e2e); Rust/Go inline `std::fs`/`os` in the per-core `Pager`. **Open:** OPFS disk-spill, the e2e in CI. → [hosts.md §3/§5/§7](spec/design/hosts.md)
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
- [ ] **(Open question, not scheduled)** low-level direct access API beneath SQL (`getValue("table", key)`) — keep the seam open, don't build yet (§9). _(size: —)_

---

## Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Several items are **ongoing obligations**
> that grow with each feature, not one-shot tasks.

- [ ] **Differential-testing harness** vs the PostgreSQL oracle (§7) — **PARTIAL.** The live-`db` oracle-import tool is built (`scripts/oracle_import.rb`; `rake corpus:import/check`; the override ledger `spec/conformance/oracle_overrides.toml`). *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated** reference provisioning §12 — never auto-provision). SQLite is deliberately not an oracle; mining its sqllogictest corpus for query *shapes* (answers from PG) is the only oracle-adjacent use. _(size: M remaining)_
- [ ] **SQLancer-style metamorphic / generative testing** — **PARTIAL.** Built so far (`scripts/norec_gen.rb`; `rake corpus:norec_sweep`, in `rake ci`): the **NoREC** slice (pushdown vs non-optimizable rewrite must agree), the **TLP** slice (ternary-logic partitioning — now including **aggregate-TLP** for `COUNT`/`SUM`/`MIN`/`MAX`, both ungrouped via scalar-subquery combination — `SUM` by `COALESCE` (grammar.md §51), `MIN`/`MAX` by `LEAST`/`GREATEST` (grammar.md §52) — and per-`GROUP BY`-group via a UNION-ALL derived-table super-aggregate), and an automatic **reducer** (`scripts/reduce.rb`, ddmin). *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator), `AVG` aggregate-TLP (deferred — its by-construction expected is an exact-`decimal` `SUM`/`COUNT` division whose scale/rounding the generator would have to replicate), and broader NoREC relations. _(size: M remaining)_
- [ ] **Corpus growth** (ongoing) — keep adding `.test` coverage as each feature lands. Two **standing obligations** (conformance.md §5/§8): (a) on the PG-comparable surface, run `rake corpus:check` and register any intentional divergence in the override ledger; (b) **when you add a query optimization or a new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does not discover new optimizations. (Future index/DISTINCT/aggregate pushdown are not yet covered.)
- [ ] **Benchmark backfill** (ongoing) — grow `bench/corpus` beyond the v1 set (benchmarks.md §11): a join benchmark (needs a second dataset table → `generator_version` bump), GROUP BY aggregate, UPDATE/DELETE throughput, miss-heavy point lookups, text/large-value-heavy rows, concurrent-reader cross-engine (PG/SQLite) + truly larger-than-pool eviction/thrashing variants (the jed-only resident and cold-population `concurrent_read_pk*_r{1,4}` pairs landed — benchmarks.md §8.1), cold-open time, durable-commit batch-size sweep. **Standing obligation** (§10): a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected benchmarks before/after. _(size: M, ongoing)_

---

## Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, [cores.md](spec/design/cores.md)).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). Any native core still passes the full conformance contract; a wrap inherits it.

- [x] **Ruby gem** — wrap the safe Rust core, shipped as a gem (conforms by construction; a distribution artifact, not an independent conformance voice). **Landed:** Slice 1 (the `cdylib` + a `fiddle`-loaded pure-Ruby gem), Slice 2 (`$N` bind params), Slice 3 (richer typed values — `BigDecimal`/`Date`/`Time`, AR-style), Slice 4 (host-loaded `JUCD`/`JTZ` bundles), and the binding-overhead benchmark (`bench/ruby`). → [ruby.md](spec/design/ruby.md), [cores.md §6](spec/design/cores.md)
  - [ ] _follow-on (each its own slice):_ a **gem prepared-statement API** (isolates the pure FFI tax from per-call parse); **`interval`/`uuid`/`bytea`** typed coercion (left as String); **distributable packaging** — a `gem install`-able native gem via `rb-sys` + precompiled platform gems (or `magnus`), replacing the in-repo `rake ruby:build` (a wrapper-module dep — needs §14 confirmation); an optional **Ruby conformance runner**. In-process Ruby host functions ride on the vectorized/batched host-function API below. _(size: L wrap)_
- [ ] **Native Node/Rust reach package experiment** — the benchmark-capable prototype is built (`impl/node`, `bench-node-rust.ts`), exact-pinned, and checksum-verified against all 53 lanes. It is not production-complete: add the full typed value/error/session API, async/worker-thread story, host-function policy, prebuilt/provenance matrix, packaging tests, and a package-name/product decision before presenting it as an alternative to native TS. Do not count it as a conformance core—it echoes Rust. Current performance decision is workload-dependent; see [cores.md §5.2](spec/design/cores.md) and [benchmarks.md §7.3](spec/design/benchmarks.md). _(size: L wrap)_
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
