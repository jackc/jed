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
  - [ ] _follow-on:_ `varchar(n)` length limits (`22001`); text `||`, `substring`. _(Runtime non-literal text→T casts + `length`/`lower`/`upper` have landed.)_
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
- [x] **Secondary indexes** (`CREATE INDEX` / `DROP INDEX`) — non-unique on-disk B-trees, maintained in the two-phase pass; the planner index-bounds a base scan on a first-column equality; `format_version` 5. → [indexes.md](spec/design/indexes.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ index ranges / multi-column prefixes; index scans for UPDATE/DELETE (keep PK pushdown today); LIMIT-streaming combination; expression/ordered/partial keys; `IF NOT EXISTS`. (All scalar key types are now encodable; only the recursive `composite` container stays a `0A000` key.)
- [x] **GIN inverted indexes** (`CREATE INDEX … USING gin`) — a second index *kind* via a type-generic operator-class seam: the **`array_ops`** opclass (one entry per distinct non-NULL element, `format_version` 12's `index_kind` byte, a `gin_entry` cost unit) accelerating `@>`/`&&`/`= ANY(col)`/array `=` for SELECT + GIN-bounded UPDATE/DELETE, over the fixed-width key-encodable element types. → [gin.md](spec/design/gin.md)
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows) / `IN` over a scalar list; the **remaining** element types — the VARIABLE-width keyables (`text[]`, `bytea[]`, `decimal[]`) need GIN term framing; `float[]` and `interval[]` are now UNBLOCKED (fixed-width keys landed) but each is its own slice — plus composite-element arrays; multi-column GIN; correlated / array-column query operands; the ordered-index equality bound for UPDATE/DELETE; the LIMIT-streaming combination; posting-list run compression; the **`jsonb_ops`** opclass and a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints** — a third index *kind* (`index_kind = 2`) whose payoff is PG exclusion constraints (`EXCLUDE USING gist (col WITH op)`, `23P01`); an operation-deterministic R-tree (a structural divergence — jed's own tree bytes), the `range_ops` + fixed-width scalar-`=` opclasses, multi-column GiST; `format_version` 21. → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ the `EXCLUDE … WHERE (predicate)` partial form; `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique index; `ALTER TABLE … ADD CONSTRAINT … EXCLUDE`; **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape.
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`RETURNING`** — `INSERT`/`UPDATE`/`DELETE … RETURNING <items>` evaluated after validation before any write; the PG-18 `old.`/`new.` row-version qualifiers landed. → [grammar.md §32](spec/design/grammar.md)
  - [ ] _follow-on:_ the `WITH (OLD AS o, NEW AS n)` aliasing form; `old.*`/`new.*`.
- [x] **`UPSERT` / `ON CONFLICT`** — `INSERT … ON CONFLICT [target] { DO NOTHING | DO UPDATE SET … [WHERE …] }`; the `excluded` pseudo-relation; column-SET or `ON CONSTRAINT name` arbiter; two-phase / all-or-nothing. → [upsert.md](spec/design/upsert.md), [grammar.md §46](spec/design/grammar.md)
  - [ ] _follow-on:_ `DO UPDATE SET col = DEFAULT` (with the `UPDATE` `SET = DEFAULT` follow-on); `INSERT INTO t AS alias`; the partial-index `WHERE index_predicate` / `COLLATE`/opclass inference decorations; relaxing the DO UPDATE PK-column assignment (`0A000`) — the standalone UPDATE re-keying has landed, but the conflict-path re-key is still deferred. → [upsert.md §10](spec/design/upsert.md)
- [ ] **Temporary tables** — `CREATE [SHARED] [TEMP|TEMPORARY] TABLE` (+ `DROP`): relations that make **zero writes to the database file** (held outside the serialized `Snapshot`, no `format_version` bump), bounded by a deterministic storage budget to keep the untrusted-SQL guarantee (§13). Namespace precludes overlaps (`42P07`); new code `54P03 temp_storage_limit_exceeded`; `allow_ddl` splits into `allow_ddl` / `allow_temp_ddl` / `allow_shared_temp_ddl`. **Landed:** slices 1–2 (session-local memory-only + database-wide shared with the two-root commit), CREATE/DROP INDEX on a temp table, serial/IDENTITY, composite-typed columns. **Open:** **slice 3 — spill-to-disk** (the resident→paged flip onto a temp `BlockStore`; the seam is already open). → [temp-tables.md](spec/design/temp-tables.md) _(size: L; deps: session model (done), storage seam (done))_
  - [ ] _follow-on:_ `ON COMMIT DELETE ROWS`/`DROP`; `IF NOT EXISTS`; `CREATE TEMP TABLE … AS SELECT`; FKs among same-kind temp tables; temporary views. → [temp-tables.md §14](spec/design/temp-tables.md)

---

## Storage maturation (§9)

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

---

## Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters + sessions
> + the CLI have landed; OPFS spill + the e2e-in-CI gap remain. Parallelizable with most feature work.

- [ ] **Storage hosts** — the five-method `BlockStore` byte device, host catalog, and decoration layering (encryption codec above the seam, replication tee below) authored in [hosts.md](spec/design/hosts.md). **Landed:** the Node `fs` host, the `FileBlockStore` extraction, and the **Browser/OPFS host** (`FileSystemSyncAccessHandle` → engine in a Web Worker, file-host parity vs goldens, gated Playwright e2e); Rust/Go inline `std::fs`/`os` in the per-core `Pager`. **Open:** OPFS disk-spill, the e2e in CI. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [x] **Session / shared-handle convergence** ✅ COMPLETE (7a–7d + the concurrent-reader bench) — fold `SharedDb`/`ReadHandle`/`WriteHandle` into `Database` + `Session` so a session *is* the configured concurrency handle, and `Database` mints concurrently-usable sessions (the deferred [session.md §2.4/§10 slice 7](spec/design/session.md) item, now designed). **Decided shape:** full rename (`SharedDb`→`Database`, the old executor handle→`Engine`); unified PG-like sessions (one writable session, lazy gate on first write — `db.read_session()`/`write_session()`/`session()`); file-backed included. All three cores in lockstep; corpus + results byte-identical (no new capability flags); the `activate`/swap is deleted; `Database` keeps a long-lived default session so the single-handle path (and every harness/example/web bridge) is unchanged. **Sub-slices:** **7a** ✅ rename-only (its own commit, green ×3); **7b** ✅ in-memory convergence (envelope `Session`→`SessionState`; the unified `Session` host handle minted by `db.read_session()`/`write_session()`/`session(opts)`, each owning a private `Engine`; the lazy gate; `activate`/swap deleted; migrated the concurrency conformance driver + stress harness + `shared`/`session`/`privileges`/`execute_script`/`lifetime_cost`/`variables` per-core tests; corpus byte-identical 281×3); **7c** ✅ file-backed sessions + the **default-session bridge** (the shared core gained the storage identity + a writer-gate `persist`; `open`/`create` return a `Database` owning a long-lived default `Session` with the `execute`/`query`/`begin`/`commit`/`rollback`/`status`/`execute_script` + envelope delegators; **per-core handle shape** — Go/TS `Database` *is* the safe core, Rust `Database` is a `!Send` wrapper over a separately-named `Send+Sync` `SharedCore` reached via `db.core()`; thread-safe-pager-under-concurrent-faults holds by construction via the `Mutex`-guarded `SharedPaging` + copy-on-write snapshots; **watermark-gated reclamation satisfied trivially** — reconstruct-on-open free-list only, so continuous within-session reclamation + active gating stays the deferred follow-on, transactions.md §8; minted sessions serialize at the file page size for cross-core byte-identity; per-core `file_sessions` tests; no format bump, no new caps. Remaining: the `Database` concurrent-reader bench); **7d** ✅ docs — the six `web/examples/*` topics × {Rust, Go, TS} rewritten to the `Database` handle + delegators (open/create→`Database`; SQL via `db.execute`/`db.query`; sessions minted with `db.session(opts)`, whose `execute`/`query` no longer take a `db` arg; `update`/`view` in Rust/Go, `begin`/`commit`/`rollback` in TS), `web/src/routes/docs/api/*` prose corrected; verified by `vite build` (Shiki) + 42-test Playwright e2e. **Concurrent-reader bench** ✅ (the slice's last item) — the ci-external `concurrent_read` kind ([benchmarks.md §8.1](spec/design/benchmarks.md)): `concurrent_read_pk_r{1,4}` mint N reader `Session`s on one shared `Database` over the resident `small` dataset; the three native cores agree on the partition-folded answer checksum (a new cross-core differential test of the concurrent read path) and scale near-linearly (Rust ~2.8×, Go ~3× at 4 readers; TS single-threaded), proving the §3 lock-free read path. jed-only; PG/SQLite cross-engine + larger-than-pool variants deferred. → [session.md §2.4/§10](spec/design/session.md), [api.md §2.5](spec/design/api.md), [transactions.md §8/§10](spec/design/transactions.md), [benchmarks.md §8.1](spec/design/benchmarks.md) _(size: L; deps: session model (done), shared handle (done), watermark (done))_
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
