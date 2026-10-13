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
- [x] **Scalar string / text functions** — PG's string surface as built-ins with code-point semantics (`length`/`substr`/`lpad`/`btrim`/`replace`/`translate`/`repeat`/`strpos`/`split_part`/`encode`/`decode`/`quote_*`/…). → [string-functions.md](spec/design/string-functions.md)
  - [ ] _follow-on:_ full-Unicode `initcap` word classification + non-ASCII titlecasing; keyword-aware `quote_ident`.
- [x] **Double-quoted identifiers + PostgreSQL identifier folding** — bare identifiers fold to lowercase, `"…"` names are exact (any UTF-8, `""` escape, `""` → `42601`); every name comparison, catalog key, and canonical name order is byte-exact; host-API names are verbatim; `nextval('…')` text parses as an identifier (`42602`); auto-names derive from exact names; the `jed_` reservation is byte-exact; `format_version` 37. → [grammar.md §3](spec/design/grammar.md)
  - [ ] _follow-on:_ PG surface still narrower around names — a quoted `EXTRACT` field, `U&"…"` Unicode-escape identifiers, and 63-byte truncation of over-long engine auto-names (an auto-name past the identifier limit can't be referenced from SQL).
- [x] **Named + optional (DEFAULT) function arguments** — PG named notation `f(name => value)` + DEFAULT params; `make_interval`, then `make_timestamp`/`make_timestamptz`; `VARIADIC` (landed as **AF6** with the array type). → [functions.md §11](spec/design/functions.md)
  - [ ] _follow-on:_ general non-integer DEFAULT values (no consumer yet — built-ins use overloads or `make_interval`-style 0-defaults); user-defined-function defaults (jed has no UDFs).

---

## The type system as the product (the differentiator, §4)

> `boolean`, `text` (collation `C` → linguistic UCA), `decimal`, `timestamp`/`timestamptz`,
> `date`, `interval`, `bytea`, `uuid`, `f32`/`f64`, **`json`/`jsonb`**, and the `array`,
> `range`, and composite containers are all done. What remains: per-type cast/function
> follow-ons, the JSON `0A000` follow-ons, and the composite-container narrowings.

- [x] **`text` + collation** — UTF-8 code-point order (type code 4), text PK/index/UNIQUE via `text-terminated-escape`; **linguistic collation** landed end-to-end: jed-owned UCA executor, `COLLATE` / per-column / per-db default, collated keys, the reference-only/vendored-tier pivot (`format_version 18`, real Unicode-17 root + `es`), and the host-loaded `JUCD` Unicode-data bundle (`db.LoadUnicodeData`). → [types.md §11](spec/design/types.md), [collation.md](spec/design/collation.md), [encoding.md §2.4](spec/design/encoding.md)
  - [x] **`varchar(n)` length limits** — a `varchar(n)` / `character varying(n)` / `string(n)` max-length typmod (the 2nd parameterized type), counted in code points; over-length assignment traps `22001` (with PG's trailing-space-truncation exception), explicit `::varchar(n)` cast silently truncates; `1 ≤ n ≤ 10485760` else `22023`; `format_version` 22 (text column/field `u32 varchar_max_len` typmod slot). → [types.md §15](spec/design/types.md)
    - [ ] _follow-on:_ `char(n)`/`character(n)` (blank-padded); `varchar(n)[]` element typmod (the `numeric(p,s)[]` narrowing); the regex `substring(text FROM pattern)` / `SIMILAR … ESCAPE` forms. _(Runtime non-literal text→T casts, any-scalar → text casts (`x::text` — [types.md §5](spec/design/types.md)), `length`/`lower`/`upper`, `substring(text, …)` with the `SUBSTRING(x FROM s FOR l)` form ([string-functions.md §4](spec/design/string-functions.md)), and text `||` (with PG's `anynonarray || text`, [string-functions.md §5](spec/design/string-functions.md)) have landed.)_
- [x] **Exact `decimal`** — *the* headline type: sign+coefficient+scale, round-half-away (settles §8), PG result scales, finite-only (documented divergence), decimal PK/index/UNIQUE via `decimal-order-preserving`; `round`/`ceil`/`ceiling`/`floor`/`trunc(x[,n])`, `gcd`/`lcm`/`width_bucket`, and the exact-numeric transcendentals `sqrt`/`ln`/`exp`/`log`/`log10`/`log(b,x)`/`power`/`pow`. → [decimal.md](spec/design/decimal.md), [encoding.md §2.5](spec/design/encoding.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods; mixed integer/decimal transcendental arguments (`power(2.0, 3)` needs an explicit cast today).
- [x] **`timestamp` / `timestamptz`** — PG instant model, i64 µs, `±infinity` first-class, timestamp PK; the host-loaded `JTZ` tz database + `AT TIME ZONE`; `date_trunc`/`EXTRACT`/cross-family casts in a zone + an observable session `TimeZone` slot. → [timestamp.md](spec/design/timestamp.md), [timezones.md](spec/design/timezones.md)
  - [ ] _follow-on:_ `to_char`/`to_timestamp`, `age`, `EXTRACT(julian …)`; PG's unit aliases for `EXTRACT`/`date_part`/`date_trunc` (`hours`, `msec`, `y`, `mon`, … — jed accepts only the canonical names, else `22023`); a separate `time` type; `timestamp(p)` precision typmods ([timezones.md §9](spec/design/timezones.md)). (`date_part` (float8), the runtime text→`timestamp`/`timestamptz` casts, offset-less `timestamptz` input read in the session zone on every path — literals, assignment, DEFAULT, bound parameters, container text ([timezones.md §9.3](spec/design/timezones.md)) — and session-zone rendering of every `timestamptz` → text — result cells through the host session render, and `timestamptz` nested in `array → text` ([timezones.md §9.5](spec/design/timezones.md)) — have landed.)
- [x] **`interval`** — PG three-field span (months/days/micros), calendar-aware arithmetic, type code 11, interval PK/index/UNIQUE/FK via the 16-byte `interval-span-i128` key. → [interval.md](spec/design/interval.md), [encoding.md §2.10](spec/design/encoding.md)
  - [ ] _follow-on:_ ISO-8601 `P…` + SQL-standard input (`'1-2'`, `'1 1:00'`, positional unitless numbers — today these fail closed with `22007`, interval.md §3); field qualifiers (`YEAR TO MONTH`) + `interval(p)`; `age`. (`EXTRACT`/`date_part` of an interval have landed — [timezones.md §9.2](spec/design/timezones.md).)
- [x] **`bytea`** — variable-width bytes, unsigned order, `\x`-hex and traditional escape-format (`\nnn`) input (`22P02`), type code 7, bytea PK/index/UNIQUE via `bytea-terminated-escape`; the binary functions `length`/`octet_length`/`bit_length`, `||`, `substr`/`substring` (+ the `SUBSTRING(x FROM s FOR l)` special form, text too), `get_byte`/`set_byte`, `get_bit`/`set_bit`, `overlay` and `position` (+ the `OVERLAY(… PLACING …)` / `POSITION(… IN …)` special forms, text too), and `btrim`/`ltrim`/`rtrim(bytea, bytea)`. → [types.md §13](spec/design/types.md), [encoding.md §2.6](spec/design/encoding.md), [string-functions.md §4](spec/design/string-functions.md)
  - [ ] _follow-on:_ whitespace between hex digit pairs in `\x` input (PG skips it; jed `22P02`); the hash functions (`md5`/`sha256` — a crypto-dependency decision, CLAUDE.md §14); untyped-literal adaptation for bytea function arguments (`get_byte('\x01', 0)` is `42883`, PG accepts it).
- [x] **`f32` + `f64` (IEEE 754)** — two-width promotion tower, the first types narrowly exempted from byte-identity (the `R` tolerant compare + exception ledger), type code 12; **float in a PK/index** via the `float-order-preserving` key; the float math functions. → [float.md](spec/design/float.md), [determinism.md](spec/design/determinism.md)
  - [ ] _deferred:_ the `width_bucket(value, thresholds[])` array-threshold variant.
- [x] **`json` / `jsonb` + SQL/JSON** — the committed XL headline feature: all non-deferred slices across all three cores, oracle-clean; spec'd across [json.md](spec/design/json.md), [jsonpath.md](spec/design/jsonpath.md), [json-sql-functions.md](spec/design/json-sql-functions.md), [json-table.md](spec/design/json-table.md); type codes 18/19/20, one `format_version` bump (v18→v19).
  - [ ] _follow-ons (deferred `0A000`, hoisted from the done slices):_ the string-**dictionary builder** (opens the [json.md §3](spec/design/json.md) door); `jsonb`-as-PK/index ([encoding.md §2.13](spec/design/encoding.md)); GIN **`jsonb_ops`** opclass for `@>`/`?`; `JSON_TABLE` explicit `PLAN` (T2); `ON ERROR/EMPTY DEFAULT <expr>` (S3); the remaining **jsonpath** surface (`like_regex` → Pike VM, item methods `.type()`/`.size()`/`.double()`/…, arithmetic, `vars`/`silent` args, the `_tz` query-function variants — P2/P3); the **verbatim-`json`** SRF / accessor variants (`json_array_elements[_text]`, `json_each[_text]`, the `->`/`#>` json overloads); `jsonb_set_lax`; `row_to_json`; in-aggregate `ORDER BY` for `json[b]_agg`.
- [x] **PostgreSQL composite types** (`CREATE TYPE name AS (…)`) — COMPLETE (S0–S6): the open `Type { Scalar | Composite(catalog-ref) }`, `CREATE`/`DROP TYPE`, nested + recursive types, storable composite column + recursive codec (`format_version` 9), `ROW(…)`, field access, element-wise compare/ORDER BY/DISTINCT/GROUP BY. Named composites only. → [composite.md](spec/design/composite.md)
  - [ ] _still narrowed (relaxable later):_ `INSERT … SELECT` / `UPDATE` of a composite column, and an anonymous `ROW(…)` *expression* (non-literal fields) in an `INSERT … VALUES` slot; an **array-of-composite** field as a key + a composite **FK** pairing (the remaining `0A000` key axes); `DEFAULT` on a composite column; runtime non-literal text→composite + `composite::text` + anonymous `ROW(…)::type` casts; the nested `ROW(ROW(…),…)`-into-column constructor.

---

## Relational depth + constraints

- [x] **Subqueries** — uncorrelated scalar, `[NOT] IN (SELECT …)`, `[NOT] EXISTS`, correlated, subqueries in UPDATE/DELETE and `INSERT ... VALUES` slots, `$N` inside a subquery, derived tables, a `VALUES` body, `LATERAL`, `x op ANY/ALL(SELECT …)`, **row-valued** subqueries (`(a, b) op | IN | op ANY/ALL (SELECT x, y …)`, with the bare `(a, b)` row constructor). → [grammar.md §26/§42/§44](spec/design/grammar.md)
  - [ ] _follow-on:_ a correlated `GROUP BY` key (`0A000`, degenerate). Correlated `ORDER BY` keys have landed as `query.order_by_correlated`.
  - [x] _follow-on:_ a **parenthesized-join FROM** (`FROM (a JOIN b ON …) [AS g]`) — unaliased groups keep their inner labels visible (chain-leading groups splice into the chain; right-hand groups plan as inline derived bodies), aliased groups are `(SELECT * FROM <join>) AS g`. → [grammar.md §15](spec/design/grammar.md)
    - [ ] _narrowing:_ a `LATERAL` item inside a right-hand group referencing relations outside it (42P01; PG allows).
- [x] **Set operations — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]`** — precedence tree (INTERSECT binds tighter), full per-column type unification, NULL-safe multiset semantics, trailing ORDER BY by name/ordinal, parenthesized operands/queries with their own ORDER BY/LIMIT. → [grammar.md §25](spec/design/grammar.md)
  - [ ] _follow-on:_ a set op in an `INSERT … SELECT` source; a `VALUES` set-op operand / top-level `VALUES` query.
- [x] **Common table expressions (`WITH`)** — named derived tables (PG hybrid inline/materialize), `WITH RECURSIVE`, data-modifying (writable) CTEs, nested `WITH`. → [cte.md](spec/design/cte.md), [recursive-cte.md](spec/design/recursive-cte.md), [writable-cte.md](spec/design/writable-cte.md)
  - [ ] _follow-on:_ recursive-CTE deferrals (`SEARCH`/`CYCLE`, a set-op / `FROM`-subquery recursive term, mutual recursion).
- [x] **`FOREIGN KEY` constraints** — column-/table-level `REFERENCES`, composite + self-reference, same-type pairing (`42804`), MATCH SIMPLE, enforced at four write sites (`23503`), plus recursive `ON DELETE`/`ON UPDATE` `CASCADE | SET NULL | SET DEFAULT` action closures (`format_version` 30's three-bit action fields). → [constraints.md §6](spec/design/constraints.md)
  - [ ] _follow-on:_ `MATCH FULL`; FK type pairing relaxed to PG's comparable-types.
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — on-disk B-trees with equality/range and multi-column-prefix bounds, expression and partial keys, bounded mutation scans, and LIMIT streaming; `format_version` 5/26/27. → [indexes.md](spec/design/indexes.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ a variable-width range/tail column (self-delimiting skip, not fixed width); ordered (`ASC`/`DESC`/`NULLS`) keys; **partial-index relaxations** — a full partial-index scan without a leading access predicate, partial OR/IN / ORDER-BY-skip / INL bounds, an ON CONFLICT partial arbiter, a general predicate-implication prover (beyond the syntactic match). (`jsonb` and composite keys are tracked separately above.)
- [x] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* via a type-generic operator-class seam: the **`array_ops`** opclass (one entry per distinct non-NULL element, `format_version` 12's `index_kind` byte, a `gin_entry` cost unit) accelerating `@>`/`&&`/`= ANY(col)`/array `=` for SELECT + GIN-bounded UPDATE/DELETE, over the fixed-width key-encodable element types. → [gin.md](spec/design/gin.md)
  Bounded LIMIT streaming completes the posting gather, then stops candidate table lookups/filtering at the window.
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows) / `IN` over a scalar list; the **remaining** element types — the VARIABLE-width keyables (`text[]`, `bytea[]`, `decimal[]`) need GIN term framing; `float[]` and `interval[]` are now UNBLOCKED (fixed-width keys landed) but each is its own slice — plus composite-element arrays; multi-column GIN; correlated / array-column query operands; posting-list run compression; the **`jsonb_ops`** opclass and a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints** — a third index *kind* (`index_kind = 2`) whose payoff is PG exclusion constraints (`EXCLUDE USING gist (col WITH op)`, `23P01`); an operation-deterministic R-tree (a structural divergence — jed's own tree bytes), the `range_ops` + fixed-width scalar-`=` opclasses, multi-column GiST; `format_version` 21. → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ the `EXCLUDE … WHERE (predicate)` partial form; `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique index; **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape.
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] }`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter; two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT`; `INSERT INTO t AS alias`; the partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed, but the conflict-path re-key is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [x] **`ALTER TABLE`** — catalog-only rename/default/nullability/constraint edits plus the
  `ADD`/`DROP COLUMN`, `ALTER COLUMN … TYPE … [USING]`, and `ADD`/`DROP PRIMARY KEY` rewrite slices
  have landed. → [alter.md](spec/design/alter.md)
  - [ ] _deferred:_ identity management (`ALTER COLUMN … ADD/DROP/SET GENERATED …`) — plausible once
    scheduled; the rest of PG's menu (OWNER/RLS/triggers/partitions/tablespaces/schemas/storage knobs) is
    deliberately out of scope, `0A000` (alter.md §6).
- [x] **Temporary tables** — session-local temp relations with zero writes to the database file,
  each domain a per-session `MemoryBlockStore` + pager with within-session compaction; CREATE/DROP
  INDEX, serial/IDENTITY, composite columns; `allow_temp_ddl`; the `max_temp_storage_bytes` `54P03`
  ceiling over the domain's page bytes. **Spill-to-disk (slice 3) landed:** past `temp_buffers` (the
  resident budget) a file host migrates the domain into an unlinked scratch file in its spill directory
  with a page cache bounded by `temp_buffers` — residency only, results/cost/`54P03`-neutral, run over
  the whole temp corpus by `mise run conformance:spill`. (The database-wide `SHARED` kind was removed
  in favor of in-memory attachments — [attached-databases.md §6](spec/design/attached-databases.md).)
  → [temp-tables.md](spec/design/temp-tables.md)
  - [ ] _follow-on:_ a host-configurable spill directory (shared with the external sort) so WASI/OPFS
    hosts can offer a temp spill target; migrating a shrunken spilled domain back into RAM.
  - [ ] _follow-on:_ `ON COMMIT DELETE ROWS`/`DROP`; `CREATE TEMP TABLE … AS SELECT`; FKs among same-kind temp tables; temporary views. → [temp-tables.md §14](spec/design/temp-tables.md)
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
  view would not show, `format_version` 39, `jed_views.check_option`).
  → [views.md](spec/design/views.md)
  - [ ] _follow-on:_ `ON CONFLICT` on an updatable view, view column defaults,
    view options and the other `ALTER VIEW` actions, materialized and recursive views, temporary views, views in or over attached
    databases, dependency tracking for
    sequences/types/collations/host functions, definitions longer than one catalog page. (View
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

- [x] **Cost as a plan input (Path B)** — landed end-to-end: exact transactional row counts,
  relation-scoped prepared-plan validity, complete candidate inventory, whole-plan estimates and
  EXPLAIN columns, costed access/order pipelines, bounded deterministic N-way join search, and
  transactional v29 `ANALYZE` statistics. The canonical contracts are
  [estimator.md](spec/design/estimator.md), [statistics.md](spec/design/statistics.md),
  [planner.md](spec/design/planner.md), and [explain.md](spec/design/explain.md).
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
  - [x] push into **inlined CTE bodies** and **below a window** — a conjunct over columns every
    window partitions by moves below the window ([planner.md §3.2](spec/design/planner.md)); a CTE
    named exactly once (by a query part), not MATERIALIZED, uncorrelated, is planned at its reference
    like a derived table and takes the same body pushdown (§3.4; its definition plans with reference
    expansion off and each body expands at most once, so chains plan linearly). EXPLAIN renders such
    a body under its `CTE Scan`. NoREC `cte_window_pushdown`.
  - [ ] _follow-on:_ push into a **widened set-operation arm** (would need the conjunct re-resolved at
    the arm's type), into each reference of a multiply-referenced `NOT MATERIALIZED` CTE (needs a
    planning-work bound: expanding every reference re-plans a chain exponentially), into a CTE
    referenced from a data-modifying part (planned only when it runs), and through a lateral
    relation.
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
- [x] **Blocking sort and hash-operator spill** — `ORDER BY`, hash `JOIN`, `GROUP BY`/aggregate and `DISTINCT` use bounded row/state buffers under `work_mem` on native file hosts. Ordered disk hash partitions preserve JOIN candidate order, aggregate fold order, first occurrence and exact costs; direct scan feeds and cost prepasses stream. Aggregate DISTINCT and growing ordered-set/hypothetical collections spill, with bounded external ordering and descriptor counts. Shared forced-spill corpus checks run in CI; `mise run bench:spill` checks wide-input results/costs and measures RSS. → [spill.md](spec/design/spill.md)
  - [ ] **Remaining query-memory owners** — Q1 (row buffers, result collectors), Q2 (operator state), Q3 (pending writes), and the storage limit (Q4a in-memory, Q4c file-backed — memory.md §8.7) have landed; remaining scalar kernels and host scratch support for OPFS/WASI remain. `work_mem` is an operator threshold, not a whole-query memory ceiling. → [memory.md](spec/design/memory.md), [hosts.md](spec/design/hosts.md)
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
- [x] **Unknown-typed function arguments** — a bare `$N` or `NULL` scalar-function argument adopts
  the family the catalog declares for its slot (one candidate decides, `text` wins among several,
  undecidable is `42725`; no preferred-type step), and a `$N` operand of `LIKE`/`ILIKE`/`~` adopts
  `text`. The `LIKE … ESCAPE` desugar's cast workaround is gone. → [functions.md §9.1](spec/design/functions.md), [api.md §5](spec/design/api.md)
  - [ ] _follow-on:_ the same rule over host-registered function signatures (an unknown argument is
    `42883` there today); the dedicated resolvers (array, range-constructor, json-set functions)
    still type their own arguments; a parameter already typed by an earlier occurrence still counts
    as unknown in a later function argument.
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
  - [x] **`Position` + `Hint`** — ✅ LANDED (all 3 cores). `Position` is the 1-based character offset of the offending token (PostgreSQL's unit): every lexer/parser error except the class-54 limits, column-reference `42703`/`42702`/`42P01`, unknown INSERT/SET targets, unknown FROM tables; script errors count from the script start. Tokens carry character positions; the AST records equality-invisible `SrcPos` on named nodes; stored text records none. `Hint` is PostgreSQL's misspelled-column (Levenshtein, `errorMissingColumn`) and table-alias suggestions. The CLI prints psql's `LINE n:` cursor and `HINT:` line. Pinned by the shared `spec/errors/position_cases.txt` (PostgreSQL-captured). → [error-fields.md §10–§11](spec/design/error-fields.md)
  - [ ] _follow-on:_ more positioned resolver errors (`42883` function lookup, `42804` operand types, `22P02` literal input, DML-target `42P01`); PostgreSQL's other hints (`42883` "You might need to add explicit type casts.", the LATERAL hints); a `DatabaseName` analog for pgx's `SchemaName` (jed qualifies by database, not schema). All additive. → [error-fields.md §7, §10.2](spec/design/error-fields.md)

---

## Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Several items are **ongoing obligations**
> that grow with each feature, not one-shot tasks.

- [ ] **Differential-testing harness** vs the PostgreSQL oracle (§7) — **PARTIAL.** The live-`db` oracle-import tool is built (`scripts/oracle_import.rb`; `mise run corpus:import` / `corpus:check`; the override ledger `spec/conformance/oracle_overrides.toml`). *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated** reference provisioning §12 — never auto-provision). SQLite is deliberately not an oracle; mining its sqllogictest corpus for query *shapes* (answers from PG) is the only oracle-adjacent use. _(size: M remaining)_
- [ ] **SQLancer-style metamorphic / generative testing** — **PARTIAL.** Built so far (`scripts/norec_gen.rb`; `mise run corpus:norec_sweep`, in `mise run ci`): the **NoREC** slice (pushdown vs non-optimizable rewrite must agree), the **TLP** slice (ternary-logic partitioning — now including **aggregate-TLP** for `COUNT`/`SUM`/`MIN`/`MAX`/`AVG`, both ungrouped via scalar-subquery combination — `SUM` by `COALESCE` (grammar.md §51), `MIN`/`MAX` by `LEAST`/`GREATEST` (grammar.md §52), `AVG` as summed sums over summed counts with a by-construction `select_div_scale` expected — and per-`GROUP BY`-group via a UNION-ALL derived-table super-aggregate), and an automatic **reducer** (`scripts/reduce.rb`, ddmin). *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator) and broader NoREC relations. _(size: M remaining)_
- [ ] **Corpus growth** (ongoing) — keep adding `.test` coverage as each feature lands. Two **standing obligations** (conformance.md §5/§8): (a) on the PG-comparable surface, run `mise run corpus:check` and register any intentional divergence in the override ledger; (b) **when you add a query optimization or a new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does not discover new optimizations.
- [ ] **Benchmark backfill** (ongoing) — grow `bench/corpus` beyond the v1 set (benchmarks.md §11): GROUP BY aggregate, miss-heavy point lookups, text/large-value-heavy rows, concurrent-reader cross-engine (PG/SQLite) + truly larger-than-pool eviction/thrashing variants (the jed-only resident and cold-population `concurrent_read_pk*_r{1,4}` pairs landed — benchmarks.md §8.1), cold-open time, durable-commit batch-size sweep. **Standing obligation** (§10): a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected benchmarks before/after. _(size: M, ongoing)_

---

## Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, [cores.md](spec/design/cores.md)).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). Any native core still passes the full conformance contract; a wrap inherits it.

- [x] **Ruby gem** — wrap the safe Rust core, shipped as a gem (conforms by construction; a distribution artifact, not an independent conformance voice). **Landed:** Slice 1 (the `cdylib` + a `fiddle`-loaded pure-Ruby gem), Slice 2 (`$N` bind params), Slice 3 (richer typed values — `BigDecimal`/`Date`/`Time`, AR-style), Slice 4 (host-loaded `JUCD`/`JTZ` bundles), Slice 5 (**host functions** — `Jed::ExtensionRegistry` with single-row and batch kernels upcalling through Fiddle closures, ABI v5, measured by the `host_function_*` triple, benchmarks.md §8.2), and the binding-overhead benchmark (`bench/ruby`). → [ruby.md](spec/design/ruby.md), [cores.md §6](spec/design/cores.md)
  - [ ] _follow-on (each its own slice):_ a **gem prepared-statement API** (isolates the pure FFI tax from per-call parse); **`interval`/`uuid`/`bytea`** typed coercion (left as String); **distributable packaging** — a `gem install`-able native gem via `rb-sys` + precompiled platform gems (or `magnus`), replacing the in-repo `mise run ruby:build` (a wrapper-module dep — needs §14 confirmation); an optional **Ruby conformance runner**; **host functions over `bytea`/`uuid`/`interval`/`json`/`jsonb`/`jsonpath`** (needs a native text→value parse at the boundary, ruby.md §5b); **`component_id`/`semantic_version` on gem host functions**, without which a gem host function can back neither an index nor a `CHECK` (extensibility.md §7.1/§8.1). _(size: L wrap)_
- [ ] **Native Node/Rust reach package experiment** — the benchmark-capable prototype is built (`impl/node`, `bench-node-rust.ts`), exact-pinned, and checksum-verified against all 53 lanes. It is not production-complete: add the full typed value/error/session API, async/worker-thread story, host-function policy, prebuilt/provenance matrix, packaging tests, and a package-name/product decision before presenting it as an alternative to native TS. Do not count it as a conformance core—it echoes Rust. Current performance decision is workload-dependent; see [cores.md §5.2](spec/design/cores.md) and [benchmarks.md §7.3](spec/design/benchmarks.md). _(size: L wrap)_
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
  - [ ] _follow-on:_ the full [compatibility.md](spec/design/compatibility.md) per-object graded reopen manifest (step 4 landed the index-only form); `REINDEX` to rebuild a stale/unusable index.

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
    completes, like `max_temp_storage_bytes`), and a net-residency measure (today a rewrite charges again).
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
