# Roadmap / TODO

> Working backlog for the engine, **grouped into sections by related area** — not a
> sequence and not a critical path. This is a living file — re-rank freely; items marked
> _(parallel)_ can proceed independently.
>
> **The live backlog is every open `- [ ]` line.** `grep '\- \[ \]' TODO.md` is the
> fastest way to find real work. A completed item is **deleted once it has no open
> follow-on** — its full design, the *why*, the error codes, the golden-fixture names,
> and the divergence ledgers live in `spec/design/*` and git history, **not here**. A
> done `[x]` item survives only as a one-line header giving an open _follow-on:_ beneath
> it context; size tags `_(size: …)_` are kept on open items only.
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

- [x] **Predicate forms** (`IN`/`BETWEEN`/`LIKE`/`ILIKE`/`CASE`, regex operators). → grammar.md §20–§23, [regex.md](spec/design/regex.md)
  - [ ] _follow-on:_ `SIMILAR TO` (deliberately excluded — the SQL-standard surface); Unicode-property char classes (`\p{…}`).
- [x] **Left-to-right short-circuit `AND` / `OR`** (PostgreSQL's qual-list NULL rule in filters, whose conjuncts are cost-ordered like PostgreSQL's). → [cost.md §3](spec/design/cost.md), [planner.md §3.0](spec/design/planner.md), `expr/short_circuit.test`, `expr/filter_conjunct_order.test`
- [x] **Scalar functions.** → [functions.md §9](spec/design/functions.md)
  - [ ] _follow-on:_ a general implicit argument-coercion pass.
- [x] **Scalar string / text functions** (code-point semantics). → [string-functions.md](spec/design/string-functions.md)
  - [ ] _follow-on:_ full-Unicode `initcap` word classification + non-ASCII titlecasing.
- [x] **Named + optional (DEFAULT) function arguments.** → [functions.md §11](spec/design/functions.md)
  - [ ] _follow-on:_ general non-integer DEFAULT values (no consumer yet — built-ins use overloads or `make_interval`-style 0-defaults); user-defined-function defaults (jed has no UDFs).
- [x] **Named windows.** → [window.md](spec/design/window.md)
  - [ ] _follow-on:_ PostgreSQL's prefix-compatible sort sharing between window definitions (window.md §11).
- [ ] **Revisit `DISTINCT ON`** — recorded out of scope in [grammar.md §11](spec/design/grammar.md), but
  it is one of PostgreSQL's most-used extensions; decide whether to implement it or keep the
  non-goal. _(size: M if implemented)_
- [ ] **Decide: SQL-language triggers and functions** — SQLite-style triggers whose bodies are plain
  SQL statements, and `CREATE FUNCTION … LANGUAGE sql`. Both stay pure, so neither conflicts with
  CLAUDE.md §13. Record each as a scheduled item or an explicit non-goal. _(size: decision; L each if built)_

---

## The type system as the product (the differentiator, §4)

> `boolean`, `text` (collation `C` → linguistic UCA), `decimal`, `timestamp`/`timestamptz`,
> `date`, `interval`, `bytea`, `uuid`, `f32`/`f64`, **`json`/`jsonb`**, and the `array`,
> `range`, and composite containers are all done. What remains: per-type cast/function
> follow-ons, the JSON `0A000` follow-ons, the composite-container narrowings, and new
> named types (enums, domains).

- [x] **`text` + collation, `varchar(n)`.** → [types.md §11/§15](spec/design/types.md), [collation.md](spec/design/collation.md)
  - [ ] _follow-on:_ `char(n)`/`character(n)` (blank-padded); `varchar(n)[]` element typmod (the `numeric(p,s)[]` narrowing).
- [x] **Exact `decimal`.** → [decimal.md](spec/design/decimal.md)
  - [ ] _follow-on:_ negative / `s>p` scale typmods.
  - [ ] _follow-on:_ `generate_series(numeric, numeric[, numeric])` — PostgreSQL has a numeric series (`generate_series(1, 3.5)`); jed's SRF is integer-only (`42883`).
- [x] **`timestamp` / `timestamptz`.** → [timestamp.md](spec/design/timestamp.md), [timezones.md](spec/design/timezones.md), [formatting.md](spec/design/formatting.md)
  - [ ] _follow-on:_ a separate `time` type; `current_timestamp(p)` / `localtimestamp(p)`; the multi-word `timestamp [(p)] with[out] time zone` spelling; `timestamp(p)[]` element typmods (the `varchar(n)[]` narrowing); zone abbreviations in `to_timestamp` input (`'… EST'` under `TZ` is `0A000` until the abbreviation-table decision, [timezones.md §15](spec/design/timezones.md), [formatting.md §5](spec/design/formatting.md)).
- [x] **`interval`.** → [interval.md](spec/design/interval.md)
  - [ ] _follow-on:_ the `INTERVAL(p) '…'` typed-literal spelling (write `'…'::interval(p)`); `interval[]` element typmods.
- [x] **`json` / `jsonb` + SQL/JSON.** → [json.md](spec/design/json.md), [jsonpath.md](spec/design/jsonpath.md), [json-sql-functions.md](spec/design/json-sql-functions.md), [json-table.md](spec/design/json-table.md)
  - [ ] _follow-ons (deferred `0A000`):_ the string-**dictionary builder** (opens the [json.md §3](spec/design/json.md) door); `jsonb`-as-PK/index ([encoding.md §2.13](spec/design/encoding.md)); `JSON_TABLE` explicit `PLAN` (T2 — needs a maintainer decision: PostgreSQL 17/18 do not implement `PLAN` (a syntax error, `42601`), so there is no PostgreSQL behavior to follow); a composite/array column in a json record function's column-definition list, and `json[b]_populate_record(set)` over an anonymous-record base (`NULL::record`) ([json-table.md §1](spec/design/json-table.md)); a NaN or ±Infinity float `PASSING` value (a `numeric` NaN/Infinity item in PG, which jed's finite `decimal` cannot hold, [json-sql-functions.md §5.4](spec/design/json-sql-functions.md)); PostgreSQL 16+'s `_` digit separators and `0x`/`0o`/`0b` radix integers in jsonpath *path text* number literals and `.**{level}` bounds (`$ ? (@ == 0x10)`, `{1_000}` — still `42601`; the SQL literals and every text → number input already take them, [jsonpath.md §2](spec/design/jsonpath.md)).
  - [ ] _follow-on:_ when a storable `jsonpath` column lands, its decoder must re-run the compiler's
    nesting gate (`XX001` on a crafted body), as the `jsonb` decoder does for `MAX_JSON_DEPTH`
    ([json.md §6.4](spec/design/json.md)).
- [x] **PostgreSQL composite types.** → [composite.md](spec/design/composite.md)
  - [ ] _still narrowed (relaxable later):_ an **array-of-composite** field as a key + a composite **FK** pairing (the remaining `0A000` key axes).
  - [ ] _follow-on:_ `DROP TYPE` dependencies from stored **expressions** — a column `DEFAULT`, `CHECK`, or expression-index key that names the type only in a cast (`(ROW(1,2)::pt).x`) records no dependency today, so `DROP TYPE pt` succeeds and leaves the expression failing when applied; PostgreSQL refuses (`2BP01`) and its `CASCADE` drops the default. Columns, fields, and views are tracked ([composite.md §7](spec/design/composite.md)).
- [ ] **Enum types** (`CREATE TYPE name AS ENUM (…)`) — PostgreSQL's ordered label sets: a named
  catalog type (the composite precedent), ordered by declaration, keyable, `ALTER TYPE … ADD VALUE`;
  a `format_version` bump. _(size: L)_
- [ ] **Domains** (`CREATE DOMAIN name AS base [DEFAULT …] [NOT NULL] [CHECK (…)]`) — a named,
  constrained base type, enforced at every assignment site and cast; squarely the "type system is
  the product" theme (CLAUDE.md §4). _(size: L)_

---

## Relational depth + constraints

- [x] **Common table expressions (`WITH`).** → [cte.md](spec/design/cte.md), [recursive-cte.md](spec/design/recursive-cte.md), [writable-cte.md](spec/design/writable-cte.md)
  - [ ] _follow-on:_ the remaining recursive-CTE deferrals (a set-op / `FROM`-subquery recursive term, mutual recursion). → [recursive-cte.md §8](spec/design/recursive-cte.md)
- [x] **Secondary indexes.** → [indexes.md](spec/design/indexes.md)
  - [ ] _follow-on (each its own slice + NoREC obligation):_ a **backward** index walk serving a
    `DESC` order from an `ASC` index (needs a unique NULL-free key, or a reversed tie-break); a
    container (range/array/composite) element after the equality prefix (needs a container slot
    skip); a full partial-index scan inside a join (sound for a NULL-rejecting predicate,
    indexes.md §9.2); prover extensions — refutation, cross-family constants, `NOT`, and a bound
    `$N`'s value (would make implication parameter-sensitive, estimator.md §6.1). (`jsonb` and
    composite keys are tracked separately above.)
  - [ ] _follow-on:_ a database qualifier on `DROP INDEX` (`REINDEX INDEX` already takes one).
- [x] **GIN inverted indexes.** → [gin.md](spec/design/gin.md)
  - [ ] _follow-on (each its own slice):_ `<@` (contained-by, broad scan + recheck — blocked on the index recording empty/NULL-array rows); a `BitmapOr` across different indexes; the **remaining** element types — composite- and `jsonb`-element arrays (each needs a non-scalar term rule); one combined gather over conjuncts on several columns of a multi-column GIN; correlated / array-column query operands; a future object/document opclass.
- [x] **GiST index access method → `EXCLUDE` constraints.** → [gist.md](spec/design/gist.md), [constraints.md §5](spec/design/constraints.md)
  - [ ] _follow-on (each its own slice + NoREC/oracle obligation):_ `DEFERRABLE` / `INITIALLY DEFERRED` (jed has no deferred-constraint machinery — its own axis); **SP-GiST** (`index_kind = 3`) and GiST KNN `ORDER BY col <-> const` (needs a distance scalar — far off); general-expression WITH operands; multi-column GiST beyond the exclusion shape; a GiST predicate-implication gate so a partial GiST index serves reads (and `CREATE INDEX … USING gist … WHERE`); `EXCLUDE` on a temporary table (`0A000` for either method).
  - [ ] _follow-on — future GiST opclasses (each gated on its type/operator surface landing first):_ **`multirange_ops`** once a multirange type lands ([ranges.md §10](spec/design/ranges.md)); an **`hstore`/dictionary-type opclass** (`@>`/`?`/`?&`/`?|`) for a future map type (a new type axis, or riding the [json.md §3](spec/design/json.md) dictionary door — which brings a **GIN** opclass too); a **`pg_trgm`-style trigram `text` opclass** accelerating similarity (`%`) / `LIKE` / `ILIKE`; an **`intarray`-style signature GiST opclass** over array columns. Each is "build it when its type / operator surface exists"; none is foreclosed by the GiST seam.
- [x] **`UPSERT` / `ON CONFLICT`.** → [upsert.md](spec/design/upsert.md)
  - [ ] _follow-on:_ the `WHERE index_predicate` through an updatable view (`0A000`); several partial
    arbiters at once (PostgreSQL arbitrates every inferable index); expression inference elements
    (`ON CONFLICT (lower(email))`). → [upsert.md §10](spec/design/upsert.md)
- [x] **`ALTER TABLE`.** → [alter.md](spec/design/alter.md)
  - [ ] _follow-on:_ the CLI dump still writes neither standalone sequences nor a `serial` column's
    owned sequence (a replayed `serial` default names a sequence that does not exist); identity columns
    are dumped (cli.md §3).
- [ ] **Generated columns** (`GENERATED ALWAYS AS (expr) STORED`) — PostgreSQL and SQLite both have
  them; jed has only identity columns. Stored text binds like a `CHECK`: persisted canonical token
  text, the [index-dependencies.md](spec/design/index-dependencies.md) purity contract, host-function
  dependencies as for `CHECK` (extensibility.md §7.1), and an entry in the
  [compatibility.md](spec/design/compatibility.md) manifest if that lands. _(size: L)_
- [x] **Temporary tables** (with spill-to-disk past `temp_buffers`). → [temp-tables.md](spec/design/temp-tables.md)
  - [ ] _follow-on:_ a host-configurable spill directory (shared with the external sort) so WASI/OPFS
    hosts can offer a temp spill target; migrating a shrunken spilled domain back into RAM.
- [x] **`CREATE TABLE … AS`.** → [create-table-as.md](spec/design/create-table-as.md)
  - [ ] _follow-on:_ array element typmods (with array columns that declare them). → [create-table-as.md §5](spec/design/create-table-as.md)
- [x] **Views** (incl. updatable views, check options, recursive and temporary views). → [views.md](spec/design/views.md)
  - [ ] _follow-on:_ materialized views, views in or over attached databases. → [views.md §12](spec/design/views.md)

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

- [x] **Runtime query-compilation investigation (Rust/Go/TS)** — research only, no production
  rollout ([investigation](spec/design/query-compilation-investigation.md)). Independent proposals
  share an [experiment contract](spec/design/execution-specialization-experiments.md); each requires
  forced-path conformance coverage, whole-query controls and a separate outcome report. (Go's
  G1/G2/G3 investigation is done — [results](bench/go-specialization/README.md).)
  - [ ] **Rust cached typed programs** — selective scan execution, plan-owned reuse and
    per-execution scratch; static-kernel controls, no native JIT dependency.
    [Proposal](spec/design/rust-expression-specialization.md).
  - [ ] **TS CSP-compatible specialization** — typed closures and static kernels, validated
    plan reuse, strict-CSP browser and ordinary Node execution; no runtime source generation.
    [Proposal](spec/design/ts-expression-specialization.md).
- [x] **Cost as a plan input (Path B).** → [estimator.md](spec/design/estimator.md), [statistics.md](spec/design/statistics.md), [planner.md](spec/design/planner.md), [explain.md](spec/design/explain.md)
  - [ ] _follow-on:_ subqueries in a top-level plain `INSERT ... VALUES` slot still plan unbound
    by the custom-plan pass (the fast path opens no boundary); custom-plan them if a workload needs
    it. ([estimator.md §6.1](spec/design/estimator.md))
  - [ ] _follow-on (`key = ANY(array)` point sets):_ the shapes that still full-scan — a promoted
    `ARRAY[...]` constructor (`ARRAY[1::i32, 3::i32]` against an `i64` key) and promoted floats
    (`f32[]` against an `f64` key). _(size: S)_
  - [ ] _follow-on:_ an uncorrelated subquery inside a derived-table body is evaluated with the
    body (per row) instead of folded before its scan opens, so its `IN` point set widens to every
    key there. _(size: S)_
  - [ ] _follow-on:_ statistics quality — pattern selectivity, extended/multi-column correlation,
    configurable targets, MCV-aware join skew, automatic analyze, distribution facts for
    composite/array/json/jsonb, GIN term statistics for `<@`, and an element-count
    histogram (PostgreSQL's `elem_count_histogram`) for array `=`.
  - [ ] _follow-on:_ broader physical search — costed outer/barrier shapes, bushy trees, parallel
    plan search, or adaptive runtime re-planning; each requires a bounded deterministic contract.
- [ ] **Parallel execution within a query** — fan one query's scans, joins, aggregation, and sorts
  out across threads over the pinned snapshot (DuckDB does this; CLAUDE.md §2 cites Rayon-grade
  parallelism as a reason to wrap Rust). Results, cost, and errors must stay identical to serial
  execution: `ORDER BY` output and the error raised must not depend on scheduling, and order-sensitive
  aggregates follow [determinism.md](spec/design/determinism.md). Needs the thread-count `CreateOptions`
  knob (below) and a design doc first. _(size: XL)_

### Planner infrastructure

- [x] **Predicate pushdown + contradiction detection.** → [planner.md §3](spec/design/planner.md)
  - [ ] _follow-on:_ push into each reference of a multiply-referenced `NOT MATERIALIZED` CTE (needs a
    planning-work bound: expanding every reference re-plans a chain exponentially), into a CTE
    referenced from a data-modifying part (planned only when it runs), and through a lateral
    relation.
  - [ ] _follow-on:_ more plan-time proofs and classifications for the safe-conjunct gate
    ([planner.md §3.5](spec/design/planner.md)) — integer `/` and `%` by a literal or bound divisor
    outside {0, −1}, a regular expression whose literal pattern compiles at plan time, the remaining
    non-trapping built-ins (numeric `trunc`/`sign`/`scale`, the range accessors, the SHA-2 digests),
    the datetime renders; and widening the hash join's own residual gate
    ([cost.md §3](spec/design/cost.md)), which still admits only bare-column comparisons.
  - [ ] _follow-on:_ plan-time **constant folding** / CSE / eliding a pushed recheck that its own
    access bound already guarantees — each removes `operator_eval` charges, so each needs its own
    explicit cost decision (the framing above), not a silent apply.

---

## Storage maturation (§9)

> The path to a **larger-than-RAM file that does not fall over** (CLAUDE.md §9): no
> full-residency assumption above the storage seam.

- [x] **Multi-process file access (the page service).** → [page-service.md](spec/design/page-service.md), [locking.md](spec/design/locking.md)
  - [ ] **Node prebuilt distribution matrix** for the `impl/ts/native-lock` adapter — ship no-install-script Node-API-8 artifacts for Linux glibc/musl x64+arm64, macOS x64+arm64, and Windows x64 with SHA-256/provenance and packaging tests. Source/dev builds already use `mise run ts:lock_build`; missing production artifacts fail closed `0A000`.
  - [ ] **Expanded multi-process fault/platform matrix** — extend the real-process handoff, timeout, pinned-reader, killed-owner, and attachment scenarios with deterministic body-sync/meta-publish fault hooks, a joiner arriving mid-compaction (the basic presence refusal + later join is `compact.process.toml`), symlink/hard-link process lanes, and every supported Windows/macOS packaging lane.
  - [ ] **Decide: configuring the remote binary and the ssh command** (maintainer). Today the CLI runs
    `ssh` from `PATH` and `jed` from the remote `PATH`, with no option for either (an environment
    variable, a flag, or a URL query are the candidates).
  - [ ] **Backup: name the host API and the CLI command.** Today each core has only a test-only
    entry point (`BackupForTesting` / `backup_for_testing` / `backupForTesting`); the public shape (a
    destination path and a form, attachments, progress) is a product decision, and cli.md has no
    backup command yet.
  - [ ] _follow-on:_ back up a file attachment, and to an OPFS destination (main only today).
  - [ ] **Phase 7 — replication.** The tee, retained log, `subscribe`, lineage and timelines (a format
    change), replicas and read replicas; amend [replication.md](spec/design/replication.md). Includes
    the encryption key-block item in the stream (encrypted.md §10).
- [x] **Encryption at rest.** → [encryption.md](spec/design/encryption.md), [encrypted.md](spec/fileformat/encrypted.md)
  - [ ] **The security review** and XAES lifetime bounds (encryption.md §9). Needs a human reviewer.
  - [ ] _follow-on:_ measure the browser's Argon2id cost (the WebCrypto page codec itself is measured).
- [x] **File compaction / shrink.** → [api.md §2.6](spec/design/api.md), [format.md *Trailing truncation*](spec/fileformat/format.md)
  - [ ] _follow-on:_ compaction options — repack half-empty leaves (today nodes are kept as-is so costs are unchanged) and change `page_size` during the rewrite. (The R-tree itself is still built from the index's keys in memory, as every commit does.) _(size: M)_
- [x] **Blocking sort and hash-operator spill.** → [spill.md](spec/design/spill.md)
  - [ ] **Remaining query-memory owners** — Q1–Q3 and the storage limits (Q4a/Q4c) have landed, as has every constructing scalar kernel (cost.md §8.1); host scratch support for OPFS/WASI remains. `work_mem` is an operator threshold, not a whole-query memory ceiling. → [memory.md](spec/design/memory.md), [hosts.md](spec/design/hosts.md)
  - [ ] _follow-on:_ bounding the interior skeleton and resident GiST R-tree once they page ([memory.md §8](spec/design/memory.md)).
- [x] **Point-lookup, cold-read, and INSERT performance work.** → [benchmarks.md](spec/design/benchmarks.md), [pager.md](spec/design/pager.md)
  - [ ] _follow-on:_ Rust's unique-dirty DELETE/rebalance transient ([transactions.md §3](spec/design/transactions.md)) for Go and TypeScript, over their INSERT
    mutation-generation tokens. Measure first: their delete path's allocation and GC profile differs
    from Rust's. _(size: M)_
- [x] **Large values — overflow pages + compression, chain sharing on rewrite.** → [large-values.md](spec/design/large-values.md)
  - [ ] _follow-on:_ keep chains on the remaining rewrite paths — `INSERT … ON CONFLICT DO UPDATE`, table rewrites (`ALTER TABLE`), and in-session rows of a table whose single root leaf stays resident after commit. Physical only (no byte or cost change). _(size: S–M)_
- [ ] **Upgrade path for older files + an on-disk stability commitment (a 1.0 blocker)** — a reader
  accepts only the current `format_version` ([format.md](spec/fileformat/format.md)), so every bump
  strands existing files, and the CLI's `--dump` needs the old binary to cross. Decide the 1.0 policy:
  an in-place upgrade on open, a `jed upgrade` that carries readers for older versions, or a frozen
  format with additive changes only. Ratify or replace the
  [compatibility.md](spec/design/compatibility.md) proposal alongside. _(size: L + decision)_

---

## Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters + sessions
> + the CLI have landed. Parallelizable with most feature work.

- [x] **Unify the create constructor.** → [api.md §2.1.1](spec/design/api.md)
  - [ ] _follow-on:_ the anticipated create-time knobs as new `CreateOptions` fields
    (a spill `temp_dir`, then a thread count);
    optionally sweep the async OPFS/browser host (`createOpfs`/`OpfsDatabase.create`) into the
    unified `create(opts)` shape.
- [x] **Unknown-typed function arguments.** → [functions.md §9.1](spec/design/functions.md), [api.md §5](spec/design/api.md)
  - [ ] _follow-on:_ the same rule over host-registered function signatures (an unknown argument is
    `42883` there today); the dedicated resolvers (array, range-constructor, json-set functions)
    still type their own arguments; a parameter already typed by an earlier occurrence still counts
    as unknown in a later function argument.
- [ ] **Integrity-check host API** — SQLite's `PRAGMA integrity_check` / `quick_check`: verify page
  checksums, tree invariants, free-list and live-count accounting, and index ↔ table agreement on a
  live database, returning structured findings. The checkers already exist in the simulation runners
  and `spec/fileformat/verify.rb`; expose them through the host API and the CLI. _(size: M)_
- [ ] **Change notifications** — host callbacks after a commit (tables touched, `txid`), plus a cheap
  "has anything committed since" counter (SQLite's update/commit hooks and `PRAGMA data_version`), for
  reactive embedded apps and cache invalidation. Must cover commits from other page-service
  clients. _(size: M)_
- [ ] **Database ↔ bytes** — open a database from a byte buffer and export one to bytes (SQLite's
  `sqlite3_serialize` / `sqlite3_deserialize`), over the in-memory `MemoryBlockStore`; useful for
  browsers, tests, and shipping prebuilt databases. _(size: S)_
- [ ] **OPFS disk-spill** — the browser host has no spill target, so sort/hash spill and temp-table
  spill stay resident there. → [hosts.md §3/§5/§7](spec/design/hosts.md)
- [x] **TS async application runtime.** → [ts-runtime.md](spec/design/ts-runtime.md)
  - [ ] _follow-on:_ public async host API gaps — changing database storage limits after open, the
    storage-byte gauge, and resetting an individual custom session variable.
  - [ ] _follow-on (measure first):_ cold/durable and other-browser public-boundary measurements,
    ARM/package-size measurements; shared-memory result transport and batch-oriented consumption
    (unimplemented investigations; transport overhead remains substantial —
    [delivery report](bench/ts/results/2026-10-19-delivery-optimization/report.md)); optional Rust
    delivery parity. Broaden runtime changes only with throughput and cancellation measurements.
- [x] **jed-migrate — the schema-migration library.** → [/migrate/design.md](migrate/design.md)
  - [ ] _follow-on ([design.md §11](migrate/design.md)):_ an all-or-nothing whole-run transaction
    mode. (A per-migration ledger table — drift detection, out-of-order, truthful status — is **not
    planned**; a potential option only on a definite need.)
- [x] **Structured error fields.** → [error-fields.md](spec/design/error-fields.md)
  - [ ] _follow-on:_ PostgreSQL's input-syntax Details for malformed `json` / array / range literals ("The input string ended unexpectedly.", "Unexpected end of input."), the `numeric field overflow` Detail, the `2BP01` Detail listing every dependent, and the LATERAL Detail for a column hidden behind a join alias; positions for collation conflicts (PostgreSQL: the second collation's expression), a `CREATE TYPE` field's type, and a `CAST(x AS t)` of a non-literal. → [error-fields.md §7, §10.2](spec/design/error-fields.md)

---

## Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Several items are **ongoing obligations**
> that grow with each feature, not one-shot tasks. (Deterministic simulation testing has landed;
> its growth obligation is in CLAUDE.md §7.)

- [ ] **Differential-testing harness** vs the PostgreSQL oracle (§7) — **PARTIAL.** The live-`db` oracle-import tool is built (`scripts/oracle_import.rb`; `mise run corpus:import` / `corpus:check`; the override ledger `spec/conformance/oracle_overrides.toml`). *Remaining:* the **bulk** bootstrap from PG's *source* test suite (gated on **user-initiated** reference provisioning §12 — never auto-provision). SQLite is deliberately not an oracle; mining its sqllogictest corpus for query *shapes* (answers from PG) is the only oracle-adjacent use. _(size: M remaining)_
- [ ] **SQLancer-style metamorphic / generative testing** — **PARTIAL.** Built: NoREC, TLP (including aggregate-TLP), and the ddmin reducer (`scripts/norec_gen.rb`, `scripts/reduce.rb`; `mise run corpus:norec_sweep`, in `mise run ci`). *Remaining:* **PQS** (pivoted query synthesis — needs an in-harness expression evaluator) and broader NoREC relations. _(size: M remaining)_
- [ ] **Corpus growth** (ongoing) — keep adding `.test` coverage as each feature lands. Two **standing obligations** (conformance.md §5/§8): (a) on the PG-comparable surface, run `mise run corpus:check` and register any intentional divergence in the override ledger; (b) **when you add a query optimization or a new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does not discover new optimizations.
- [ ] **Coverage-guided fuzzing of SQL text** — untrusted SQL must never crash a core (CLAUDE.md §13;
  a panic is a denial of service), but the only fuzzer today mutates on-disk pages
  (`impl/go/corruption_fuzz_test.go`). Add parser → resolver → executor fuzz targets in every core
  (Go's built-in `testing.F`; a Rust/TS fuzzer is a tooling dependency needing §14 confirmation),
  seeded from the corpus, with crashes reduced into regression `.test` entries. _(size: M)_
- [ ] **Benchmark backfill** (ongoing) — grow `bench/corpus` beyond the v1 set (benchmarks.md §11): concurrent-reader cross-engine (PG/SQLite) + truly larger-than-pool eviction/thrashing variants (the jed-only resident and cold-population `concurrent_read_pk*_r{1,4}` pairs landed — benchmarks.md §8.1), durable-commit batch-size sweep. **Standing obligation** (§10): a perf-relevant feature lands with a benchmark; a perf-sensitive change runs the affected benchmarks before/after. _(size: M, ongoing)_

---

## Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, [cores.md](spec/design/cores.md)).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). Any native core still passes the full conformance contract; a wrap inherits it.

- [x] **Ruby gem** (wraps the safe Rust core). → [ruby.md](spec/design/ruby.md), [cores.md §6](spec/design/cores.md)
  - [ ] _follow-on (each its own slice):_ a **gem prepared-statement API** (isolates the pure FFI tax from per-call parse); **`interval`/`uuid`/`bytea`** typed coercion (left as String); **distributable packaging** — a `gem install`-able native gem via `rb-sys` + precompiled platform gems (or `magnus`), replacing the in-repo `mise run ruby:build` (a wrapper-module dep — needs §14 confirmation); an optional **Ruby conformance runner**; **host functions over `bytea`/`uuid`/`interval`/`json`/`jsonb`/`jsonpath`** (needs a native text→value parse at the boundary, ruby.md §5b); **`component_id`/`semantic_version` on gem host functions**, without which a gem host function can back neither an index nor a `CHECK` (extensibility.md §7.1/§8.1). _(size: L wrap)_
- [ ] **Native Node/Rust reach package experiment** — the benchmark-capable prototype is built (`impl/node`, `bench-node-rust.ts`), exact-pinned, and checksum-verified against all 53 lanes. It is not production-complete: add the full typed value/error/session API, async/worker-thread story, host-function policy, prebuilt/provenance matrix, packaging tests, and a package-name/product decision before presenting it as an alternative to native TS. Do not count it as a conformance core—it echoes Rust. Current performance decision is workload-dependent; see [cores.md §5.2](spec/design/cores.md) and [benchmarks.md §7.3](spec/design/benchmarks.md). _(size: L wrap)_
- [ ] **Python package** — the largest audience for embedded databases (the `sqlite3` module,
  DuckDB), and absent from the plan. Likely a wrap of the safe Rust core (PyO3 or the C API below —
  either needs §14 confirmation), with a DB-API 2.0 (PEP 249) surface, `Decimal`/`datetime`/`uuid`
  value mapping, and prebuilt wheels. Record the native-vs-wrap call in [cores.md](spec/design/cores.md). _(size: L wrap)_
- [ ] **Public, stable C API + header** — the Ruby gem already loads a Rust `cdylib`; promoting that
  boundary to a documented, versioned C ABI lets PHP, Dart/Flutter, Elixir, and others bind cheaply
  (SQLite's C API is the reason it is everywhere). _(size: L)_
- [ ] **C#** core — **lean native** (value-type generics, `Span<T>`, NativeAOT → near-Rust speed *and* a clean pure-managed NuGet package; in-process host functions). Strongest native candidate. _(size: XL native / L wrap; §2)_
- [ ] **Swift** core — **lean wrap** (UniFFI + XCFramework over the safe Rust core: Rust speed, well-trodden Apple packaging, untrusted-query safety preserved §13). Native only if hot-path per-row host functions make the FFI upcall tax dominant. _(size: L wrap / XL native; §2/§13)_
- [ ] **Java** core — **conflicted**: wrap for performance (pre-Valhalla boxing + JIT warmup hurt a native core), native for clean pure-JAR packaging + in-process host functions (no JNI/upcall tax). Decide at scheduling time; Valhalla shifts it toward native. _(size: XL native / L wrap; §2)_

### Host extensions

- [x] **Runtime function registry.** → [extensibility.md §5](spec/design/extensibility.md)
  - [ ] _follow-on:_ built-in type-vtable dogfood (Fork A).
- [x] **Host scalar functions over existing types, with the batched ABI.** → [extensibility.md §4.2 / §5.1](spec/design/extensibility.md)
  - [ ] _follow-on:_ batch at the remaining row-evaluation sites — index-driven streaming scans (carry the per-row point-lookup charge to replay), streaming joins, `WHERE`/`ON` filters inside blocking scans and joins, aggregate arguments, `INSERT … VALUES`, `UPDATE … SET`, `RETURNING`, index build/maintenance; and calls with arguments beyond column/constant/param. The Node/Rust package's boundary remains to be measured (benchmarks.md §8.2). _(size: M)_
  - [ ] _follow-on:_ non-strict host functions, host functions over container args, and runtime **enforcement** of the recorded `cross_core` declaration (taint/containment of non-cross-core results — no runtime taint mechanism exists yet, matching how `float` is handled at the spec layer).
- [x] **Host functions in persisted indexes** (Model B, plus `REINDEX`). → [extensibility.md §7 / §8.1 / §11](spec/design/extensibility.md)
  - [ ] _follow-on:_ the full [compatibility.md](spec/design/compatibility.md) per-object graded reopen manifest (step 4 landed the index-only form).
- [ ] **Host-defined types and operator classes** — the designed but unscheduled steps 5–8 of
  [extensibility.md §14](spec/design/extensibility.md): opaque host scalar types (Storable /
  Equatable / Ordered) with derived `array<host-type>` and the graded reopen verdict for every object
  kind; keyable host types (an order-preserving encoder + the invariant harness); host GIN/GiST
  operator classes behind the unified seam; and SQL syntax to choose a non-default operator class.
  _(size: XL across the steps)_
- [ ] **Host aggregate and table-valued functions** — not yet designed. Host aggregates (state,
  transition, final, as a batched kernel) and host functions that return rows usable in `FROM` (SQLite's
  virtual tables, DuckDB's table functions — a host-extension row source, CLAUDE.md §9/§13). Design
  in extensibility.md first. _(size: L each)_

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
- **Full-text search.** PostgreSQL (`tsvector`/`tsquery` + GIN) and SQLite (FTS5) both ship it. In
  jed it would be a type pair, a tokenizer/stemmer data bundle loaded through the `Environment` (the
  collation precedent), and a GIN operator class. Large; not scheduled.
- **A logical changeset stream.** Replication ships block deltas
  ([replication.md](spec/design/replication.md)); a logical stream (compact wire, heterogeneous
  consumers) is a separate higher-layer door, not foreclosed, not scheduled.
