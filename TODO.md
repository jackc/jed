# Roadmap / TODO

> Working backlog for the engine. Ordered **roughly** by dependency → importance →
> difficulty, grouped into phases. This is a living file — re-rank freely. The phases
> are a suggested critical path, **not** rigid gates; items marked _(parallel)_ can
> proceed independently.
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

Difficulty key: **S** ≈ hours · **M** ≈ a day · **L** ≈ multi-day · **XL** ≈ a project.

---

## Phase 0 — Meta / housekeeping

- [ ] **Name the project.** CLAUDE.md says the name is *TBD*; `abide` is the de-facto
      placeholder (Cargo crate, Go module, `.adb` file extension). Decide: ratify `abide`
      or pick a name, then sweep the codebase + docs. _(size: S)_

---

## Phase 1 — Foundations: spec backfill + the expression substrate

> Highest leverage, mostly low difficulty. These unblock nearly every later feature and
> close gaps in the *canonical artifact itself* (two spec dirs are still empty).

- [x] **Backfill the EBNF grammar.** The grammar is the shared contract the hand-written
      parsers conform to (§5/§6); three parsers previously existed with no authored grammar.
      Done: [spec/grammar/grammar.ebnf](spec/grammar/grammar.ebnf) (W3C-style EBNF) covers the
      already-implemented surface (CREATE TABLE / INSERT / SELECT / WHERE / ORDER BY / UPDATE /
      DELETE / CAST), with the *why* in [spec/design/grammar.md](spec/design/grammar.md). Grow
      it per feature. _(size: M; §6)_
- [x] **Author the function / operator catalog.** Operator **result types** (e.g. type of
      `int32 + int32`) and NULL behavior live here as data (§5). Done:
      [spec/functions/catalog.toml](spec/functions/catalog.toml) backfills the comparison
      operators (`= < > <= >=`) and null tests (`IS [NOT] NULL`) the cores hardcode, with a
      family-based schema that references the promotion tower rather than restating it, a
      coherence checker ([spec/functions/verify.rb](spec/functions/verify.rb), wired into
      `rake verify`), and the *why* in [spec/design/functions.md](spec/design/functions.md).
      Prerequisite for all arithmetic/boolean/function work. _(size: M; §5)_
- [x] **Decide & build the codegen "middle path"** for the function catalog (§5). Decided:
      codegen emits **data only** (a per-language operator descriptor table from
      `spec/functions/catalog.toml`); the parser/executor/evaluator that consume it stay
      hand-written (§5 forbids codegenning those). Done: [scripts/gen_catalog.rb](scripts/gen_catalog.rb)
      (`rake codegen`) emits `impl/{rust/src,go,ts/src}/operators.{rs,go,ts}` (checked-in,
      `@generated`); a `rake verify` drift gate + per-core cross-check tests keep them in
      sync; the *why* is in [spec/design/codegen.md](spec/design/codegen.md). Forward: extend
      the generator to types/errors. _(size: M; §5)_ _(parallel)_
- [x] **Resolve integer-literal typing.** Decided **context-adaptive**: a bare integer
      literal is an *untyped constant* that adapts to its context (the column on
      INSERT/UPDATE/comparison, the CAST target) and traps `22003` when its value does not
      fit, defaulting to int64 with no context. Authored in
      [spec/design/types.md](spec/design/types.md) §6 (conformance.md §7 flipped to
      resolved); the one new code path is a literal range-check in each core's WHERE-predicate
      resolution (so `WHERE small = 100000` now traps instead of silently matching nothing),
      pinned by [spec/conformance/suites/types/literals.test](spec/conformance/suites/types/literals.test).
      _(size: S; §4)_
- [x] **General expression evaluator.** Done: a unified recursive `Expr` (Column/Literal/
      Cast/Unary/Binary/IsNull) replaced the split `Operand`/`Predicate`/`SelectExpr`, with a
      one-function-per-level precedence parser and a recursive resolve→eval in all three cores,
      shared by WHERE and the SELECT list (parenthesization included). Landed **together** with
      the next three items as one slice (the substrate is only testable with operators on it);
      function-call syntax stays deferred (no scalar functions defined yet). _(was: L; §5)_
- [x] **Integer arithmetic operators** `+ - * / %` and unary `-`, trap-on-overflow (`22003`)
      at the **result type's** boundary (`int16+int16` traps at int16), defined `/`/`%`-by-zero
      (`22012`); result types from the promotion tower. Authored in the catalog (kind
      `arithmetic`, result `promoted`) + `spec/conformance/suites/expr/{arithmetic,unary_minus}.test`.
      _(was: M; §4/§8)_
- [x] **`boolean` scalar type** — **expression-only** this slice (the first non-integer scalar):
      `TRUE`/`FALSE` literals, comparison/logical results, projectable in SELECT, consumed by
      WHERE; render tag `B` (`true`/`false`). It is **not yet a storable column type** (see the
      storable-boolean follow-on in Phase 3). _(was: M; §4)_
- [x] **Logical connectives `AND` / `OR` / `NOT`** with three-valued (Kleene) truth tables —
      `AND`/`OR` are `null = "kleene"` (a dominant operand absorbs NULL), `NOT` propagates.
      Coverage in `spec/conformance/suites/expr/{logical,precedence}.test`. _(was: M; deps: boolean ✓)_
- [x] **`IS [NOT] DISTINCT FROM`** — NULL-safe equality. Done: a new `null = "null_safe"`
      operator pair in [spec/functions/catalog.toml](spec/functions/catalog.toml) (same
      `integer × integer` `promote` contract and `boolean` result as `=`; only the NULL
      handling is total — `NULL IS NOT DISTINCT FROM NULL` is TRUE, the result is never
      unknown). The shared `IS` `NOT`? prefix dispatches on `NULL` vs `DISTINCT FROM` in the
      grammar ([spec/grammar/grammar.ebnf](spec/grammar/grammar.ebnf) `comparison`,
      non-associative) and in all three parsers; one `not_distinct_from` value primitive +
      one resolved node per core (reusing the `=` operand resolution). Pinned by
      [spec/conformance/suites/expr/is_distinct_from.test](spec/conformance/suites/expr/is_distinct_from.test)
      (`query.is_distinct_from`, in the `expression` profile). The why is in
      [functions.md](spec/design/functions.md) §3 / [types.md](spec/design/types.md) §4.
      _(size: S; deps: boolean ✓)_
- [x] **Cost-accounting seam (design early, enforce later).** Done (the **seam**; enforcement
      still deferred): a deterministic cost counter (`Meter`) threads through the executor /
      expression evaluator / storage reads in all three cores, accruing from a data-defined unit
      schedule ([spec/cost/schedule.toml](spec/cost/schedule.toml): `storage_row_read`,
      `row_produced`, a uniform `operator_eval`; codegen'd to `costs.{rs,go,ts}` via
      [scripts/gen_costs.rb](scripts/gen_costs.rb), drift-gated by `rake verify`). Cost is exposed
      on `Outcome` and is a cross-core contract: the `# cost: N` corpus directive
      ([spec/conformance/suites/expr/cost.test](spec/conformance/suites/expr/cost.test), gated by
      the `resource.cost_metering` capability) asserts the **byte-identical** accrued cost in
      Rust, Go, **and** TS. The accrual rules (interior nodes only, no short-circuit, pre-order)
      and the deferred bits are in [spec/design/cost.md](spec/design/cost.md). **Still deferred:**
      the caller-set **max-cost ceiling + deterministic abort** (and its error code) — designed so
      `Meter.charge` is the single chokepoint where it slots in; a real `page_read` unit; and
      per-operator `cost` weights. _(was: M seam / L full enforcement; §13)_ _(parallel)_

---

## Phase 2 — Make it feel like SQL (core query/DML completeness)

> Builds directly on the Phase 1 expression substrate. High importance, mostly M.

- [x] **Select-list expressions + `*` + column aliases (`AS`).** Select-list expressions and
      `*` already worked; this added explicit `AS` aliases and, with them, **output column
      naming** as a cross-core contract. Done: the naming rule (bare column → catalog canonical
      name; `expr AS alias` → alias; `*` → column names; any other expression → the fixed
      `?column?`) authored in [spec/design/grammar.md](spec/design/grammar.md) §8 + the
      `select_item` production in [spec/grammar/grammar.ebnf](spec/grammar/grammar.ebnf); the
      query `Outcome` now carries `column_names` in all three cores (replacing the dead
      `column_count`), with aliases parsed as output-only labels (invisible to WHERE/ORDER BY);
      and a new `# names:` conformance directive (mirroring `# cost:`,
      [conformance.md](spec/design/conformance.md) §1) asserts the byte-identical names in Rust,
      Go, **and** TS, pinned by
      [spec/conformance/suites/query/select_list.test](spec/conformance/suites/query/select_list.test)
      (capabilities `query.column_alias` + `query.select_star`). _(size: M; deps: expression evaluator)_
- [ ] **`LIMIT` / `OFFSET`.** _(size: S)_
- [ ] **Richer `ORDER BY`** — multiple keys, per-key `ASC`/`DESC`, `NULLS FIRST|LAST`
      (the physical NULLs-first order is ratified; this is the SQL-level override, types.md §4).
      _(size: M)_
- [ ] **`DISTINCT`.** _(size: S–M)_
- [ ] **Predicate forms** — `IN (list)`, `BETWEEN`, `LIKE` (text-dependent), `CASE`
      expressions. _(size: M; LIKE deps: text type)_
- [ ] **Aggregates** `COUNT` / `SUM` / `MIN` / `MAX` / `AVG` + **`GROUP BY`** + **`HAVING`**.
      `AVG`/`SUM` interact with overflow & with decimal — sequence after `decimal` or define
      integer-only semantics first. _(size: L; deps: expression evaluator)_
- [ ] **Multi-row `INSERT`** (`VALUES (..),(..)`) and **`INSERT ... SELECT`**. _(size: S/M)_
- [ ] **`DROP TABLE`.** _(size: S)_

---

## Phase 3 — The type system as the product (the differentiator, §4)

> "Like SQLite, but with a *real* type system." Each is a vertical slice that forces a
> §8 divergence decision into the open. `text` then `decimal` are the headline items.

- [ ] **Storable `boolean` column type.** `boolean` is expression-only today (Phase 1); make
      it a *column* type: allow `CREATE TABLE t(flag boolean)` and `INSERT`/store/retrieve
      (and `CAST … AS boolean`), currently `0A000`. Touches the byte-exact storage surface —
      add on-disk type code `4` + a golden round-trip fixture, the `bool-byte` key-encoding
      vectors (the rule is already recorded in `scalars.toml`), and a `boolean × boolean`
      comparability rule. Cleanly additive (old files keep working). _(size: M; §4/§8/§9)_
- [ ] **`text` + ONE defined collation** (byte/codepoint order to start — §8). Unblocks
      `LIKE`, string functions, realistic schemas. UTF-8 vs UTF-16 across cores is a
      divergence hotspot — TS already proved UTF-8 names. _(size: L; §4/§8)_
- [ ] **Exact `decimal`** — *the* headline type. Forces decimal **rounding mode + scale**
      and keeps binary floats out of the comparison/text paths (§8). `numeric.c` (Postgres)
      is the reference. Hard, high-value. _(size: XL; §4/§8)_
- [ ] **`timestamp` / `timestamptz`.** Forces a defined epoch, range, and tz model;
      determinism-sensitive (no wall-clock in tests). _(size: L; §4)_
- [ ] **`bytea`.** Order-preserving encoding is straightforward (raw bytes). _(size: M; §4)_
- [ ] **`json` / `jsonb`** — optional headline feature (§1). Large surface. _(size: XL; §4)_
- [ ] **Float policy decision.** §8 deliberately keeps `f64` out of compare/text-output
      paths. Decide if floats ever exist, and if so how rendered. _(size: S decision / L if built; §8)_

---

## Phase 4 — Relational depth + constraints

> The meaty planner/executor work and the rest of the integrity story.

- [ ] **`JOIN`** — inner first, then `LEFT`/`RIGHT`/`FULL OUTER`, `CROSS`. Needs multi-table
      FROM + a join executor. _(size: L; deps: expression evaluator)_
- [ ] **Subqueries** — scalar, `IN (subquery)`, `EXISTS`, then correlated. _(size: L; deps: joins)_
- [ ] **Set operations** — `UNION [ALL]`, `INTERSECT`, `EXCEPT`. _(size: M)_
- [ ] **Constraints** — `NOT NULL`, `DEFAULT`, `UNIQUE`, `CHECK`, **composite `PRIMARY KEY`**
      (key encoding already composes — types.md §7), `FOREIGN KEY`. NOT NULL/DEFAULT are
      easy and could be pulled into Phase 2; UNIQUE/CHECK/FK are heavier. _(size: S→L each)_
- [ ] **Secondary indexes** (`CREATE INDEX`) — also a planner + storage concern (index
      pages, index maintenance on write). _(size: L; deps: storage maturation)_
- [ ] **`RETURNING`** clause; **`UPSERT` / `ON CONFLICT`**. _(size: M; deps: UNIQUE)_
- [ ] **Relax the UPDATE narrowings** — allow assigning a `PRIMARY KEY` column (currently
      `0A000`; means the storage key can change). Documented as relaxable (§11 step 6).
      _(size: M; deps: transactions for clean re-keying)_

---

## Phase 5 — Transactions & the §3 commit model

> The real concurrency story. Currently only **per-statement** atomicity exists (UPDATE's
> two-phase pass); the §3 single-writer staging buffer is still future. Couples tightly
> with Phase 6 (the staging buffer *is* the in-memory pending set the COW commit flushes).

- [ ] **In-memory staging area / pending write set** — accumulate a writer's changes off to
      the side, last-committed state continuously readable (§3). _(size: L; §3)_
- [ ] **`BEGIN` / `COMMIT` / `ROLLBACK`** — multi-statement transactions on top of the
      staging buffer. _(size: L; deps: staging area)_
- [ ] **Reader/writer concurrency semantics + tests** — readers never block except during
      the commit root-swap window; single writer (§3). Determinism in tests is delicate.
      _(size: L; §3)_

---

## Phase 6 — Storage maturation (§9)

> Can lag the feature work until write volume makes whole-image rewrites costly. The
> forward-compatible hooks (two meta slots, checksum, root pointer, write-ordering) are
> already in place.

- [ ] **Incremental copy-on-write commit** — replace the whole-image serialize with
      dirty-page-only writes + meta-page root swap (§9, storage.md §4). _(size: XL; deps: staging area)_
- [ ] **Free-list / page reclamation** — reuse pages the new root no longer references
      (not version GC; still not MVCC). _(size: L; deps: incremental commit)_
- [ ] **B-tree interior pages + slotted page layout** — current layout is a flat sorted
      record chain (storage.md §6). Needed for scale. _(size: XL; deps: incremental commit)_
- [ ] **Crash-recovery hardening** — torn-meta fixtures exist; expand durability/recovery
      tests. WAL is deferred (COW + root-swap gives atomicity without one). _(size: M; §9)_

---

## Phase 7 — Embedding / host API surface

> The north star is an **embeddable library** (§1). Today the only entry point is
> `Execute(db, sql)`. Parallelizable with most feature work.

- [ ] **Formal public API** — open/close a database file, prepare a statement, execute,
      iterate result rows, statement/lifecycle + structured error surface — designed to be
      *the same shape* across cores. _(size: L; §1)_
- [ ] **Parameterized queries (`$1`)** end-to-end — the `WHERE pk = $1` API implied by
      §11 step 5. Per-impl surface (corpus stays literal-only, conformance.md §1.2). _(size: M)_
- [ ] **Storage hosts** — Node `fs` host exists; build the **browser/OPFS** host
      (`FileSystemSyncAccessHandle`) and confirm native file-host parity (§9, storage.md §2).
      _(size: L; §9)_
- [ ] **(Open question, not scheduled)** low-level direct access API beneath SQL
      (`getValue("table", key)`) — keep the seam open, don't build yet (§9). _(size: —)_

---

## Phase 8 — Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Some pairs with earlier phases.

- [ ] **Differential-testing harness** vs PostgreSQL/SQLite oracles to bootstrap corpus
      cheaply (§7). Gated on **user-initiated** reference provisioning (§12) — never
      auto-provision. Valuable as soon as `text`/`decimal` widen the surface. _(size: L; §7)_
- [ ] **SQLancer-style metamorphic / generative testing** — finds logic bugs by
      synthesizing queries with known-correct answers. Explicitly *later* (§7). _(size: L; §7)_
- [ ] **Result-type assertion directive** — assert a column's precise declared type
      (`int16` vs `int32`) beyond the `I`/`T`/`R` render tag (deferred, conformance.md §7).
      _(size: S; §7)_
- [ ] **Corpus growth** — keep adding `.test` coverage as each feature lands (ongoing).

---

## Phase 9 — Portability: more native cores (§2)

> After the spec has hardened further. These mostly prove portability + expand coverage
> rather than surfacing new ambiguities (TS already did the heavy lifting).

- [ ] **Java** core (native). _(size: XL; §2)_
- [ ] **C#** core (native). _(size: XL; §2)_
- [ ] **Swift** core — **prefer embedding/wrapping the Rust core** (good Swift↔Rust interop,
      and Rust is memory-safe so it preserves the untrusted-query safety property, §13). Fall
      back to a **native** core only if Rust embedding proves insufficient — the one
      explicitly-allowed deliberate exception to the native rule (§2). _(size: L wrap / XL native; §2)_

---

## Ordering rationale & open tensions (for iteration)

- **Why Phase 1 first:** two canonical spec dirs (`grammar/`, `functions/`) are still
  empty, and a general expression evaluator is the prerequisite for almost everything in
  Phases 2 & 4. Cheap to do, unblocks the most.
- **Why the type system (Phase 3) is its own phase, not earlier:** it's *the product*, but
  most type work depends on the expression/operator substrate from Phase 1, and `decimal`
  (XL) shouldn't gate the SQL-shape features in Phase 2.
- **Tensions to decide:**
  - `NOT NULL` / `DEFAULT` are fundamental and easy — pull them into Phase 2?
  - `JOIN`s are arguably core SQL — promote ahead of aggregates?
  - Transactions (Phase 5) could move earlier if multi-statement atomicity is wanted
    before storage maturation; it's only placed here because it couples with Phase 6.
  - `text` vs `decimal` ordering within Phase 3 — `text` is the bigger immediate unlock
    (LIKE, string fns); `decimal` is the bigger headline.

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
