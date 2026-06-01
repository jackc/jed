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
- [ ] **Author the function / operator catalog** (`spec/functions/` is empty). This is
      where operator **result types** live (e.g. type of `int32 + int32`) and NULL
      behavior, as data (§5). Define the schema + the comparison operators that current
      code hardcodes. Prerequisite for all arithmetic/boolean/function work. _(size: M; §5)_
- [ ] **Decide & build the codegen "middle path"** for the function catalog — generate
      per-language operator stubs from the shared data rather than hand-writing N times
      (§5). Pairs with the catalog above. _(size: M; deps: function catalog; §5)_ _(parallel)_
- [ ] **Resolve integer-literal typing** — currently flagged *open* (conformance.md §7):
      is a bare `1000` an `int16`, the smallest fitting type, or context-adapted? Decide in
      `spec/types/`, then add corpus coverage. Blocks clean expression semantics. _(size: S; §4)_
- [ ] **General expression evaluator.** Executor today handles bare columns + `CAST` +
      single comparisons. Build a real nested-expression tree (operators, function calls,
      parenthesization) in WHERE and the SELECT list. The single biggest unlock for query
      features. _(size: L; deps: function catalog; §5)_
- [ ] **Integer arithmetic operators** `+ - * / %` with trap-on-overflow and defined
      `/`-by-zero / `%`-by-zero (`22012`) behavior; result types from the promotion tower.
      _(size: M; deps: expression evaluator, function catalog; §4/§8)_
- [ ] **`boolean` scalar type.** First non-integer type. Unblocks proper predicate values
      and the logical connectives below. Forces a render tag beyond `I`/`T`/`R`. _(size: M; §4)_
- [ ] **Logical connectives `AND` / `OR` / `NOT`** over predicates, with three-valued
      truth tables (§4). Explicitly waiting on `boolean` (conformance.md §7). _(size: M; deps: boolean)_
- [ ] **`IS [NOT] DISTINCT FROM`** — NULL-safe equality (design already references it,
      types.md §4). _(size: S; deps: boolean)_
- [ ] **Cost-accounting seam (design early, enforce later).** For safely running untrusted
      queries (CLAUDE.md §13): thread a **deterministic** cost counter through the executor /
      expression evaluator / storage reads *now*, while the executor is still small — every
      page read, row produced, and function/operator evaluation accrues a defined cost. Cost
      must be deterministic and **identical across cores** (a §8-style hotspot; assertable in
      the corpus). The caller-set **max-cost ceiling + deterministic abort** can land later;
      the seam is what must be baked in early. _(size: M seam / L full enforcement; §13)_ _(parallel)_

---

## Phase 2 — Make it feel like SQL (core query/DML completeness)

> Builds directly on the Phase 1 expression substrate. High importance, mostly M.

- [ ] **Select-list expressions + `*` + column aliases (`AS`).** Today SELECT takes bare
      columns (and CAST). _(size: M; deps: expression evaluator)_
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
      (key encoding already composes — types.md §6), `FOREIGN KEY`. NOT NULL/DEFAULT are
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
