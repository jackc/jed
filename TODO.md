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
5. **PostgreSQL behavior by default** — where the feature has a choice and one option matches
   PostgreSQL, take it unless there's a documented overriding reason (CLAUDE.md §1). Any
   deliberate divergence from PG is recorded in the relevant spec doc.

Difficulty key: **S** ≈ hours · **M** ≈ a day · **L** ≈ multi-day · **XL** ≈ a project.

---

## Phase 0 — Meta / housekeeping

- [x] **Name the project.** Settled on **`jed`** (was the placeholder `abide`). Swept the
      codebase + docs: Cargo crate, Go module/package, TS package, the on-disk magic
      (`ABDB` → `JEDB`), the file extension (`.adb` → `.jed`), and the devcontainer
      identifiers (`devstate-shared-jed`, `/workspaces/jed`). _(size: S)_

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
      and the accrual rules are in [spec/design/cost.md](spec/design/cost.md). The caller-set
      **max-cost ceiling + deterministic abort** (`54P01`) has since **landed** (Phase 7, all 3
      cores) via `Meter::guard()` at the work loops; a real `page_read` unit landed (P6.3). **Still
      deferred:** per-operator `cost` weights. _(§13)_ _(parallel)_

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
- [x] **`LIMIT` / `OFFSET`.** Done: `LIMIT n` caps and `OFFSET m` skips result rows, the two
      clauses accepted in **either order**, each at most once (a duplicate is `42601`). The count
      is a **non-negative integer literal** (not a general expression); a negative value is a
      deterministic parse-time data error — **`2201W`** (LIMIT) / **`2201X`** (OFFSET), the
      PostgreSQL SQLSTATEs, added to [spec/errors/registry.toml](spec/errors/registry.toml). The
      slice runs **after `ORDER BY`, before projection**, so excluded rows are scanned + filtered
      but charge no `row_produced`/projection cost — a cross-core determinism contract pinned by
      the `# cost:` directive. Authored in [spec/grammar/grammar.ebnf](spec/grammar/grammar.ebnf)
      (`limit_offset`) + [grammar.md §9](spec/design/grammar.md), [cost.md §3](spec/design/cost.md),
      and capabilities `query.limit` + `query.offset` in
      [manifest.toml](spec/conformance/manifest.toml), all three cores in lockstep, pinned by
      [spec/conformance/suites/query/limit_offset.test](spec/conformance/suites/query/limit_offset.test).
      _(size: S)_
- [x] **Richer `ORDER BY`** — multiple keys, per-key `ASC`/`DESC`, `NULLS FIRST|LAST`. Done:
      `order_by` is now `sort_key ("," sort_key)*` with each key a **bare column** (ordinal /
      expression / alias keys still deferred), an optional direction, and an optional
      `NULLS FIRST|LAST`. The per-key comparator **decouples** NULL placement from the
      value-direction flip, so an explicit `NULLS FIRST|LAST` overrides regardless of
      direction; with no clause the default **follows the ratified physical order** —
      `ASC` → NULLs last, `DESC` → NULLs first (NULL = largest, the **PostgreSQL model**),
      resolved at parse time. The sort stays **unmetered**
      (cost.md §3), so the `# cost:` math is unchanged. Authored in
      [grammar.ebnf](spec/grammar/grammar.ebnf) (`order_by` / `sort_key`) +
      [grammar.md §10](spec/design/grammar.md), [types.md §4](spec/design/types.md), the new
      `query.order_by_keys` capability in [manifest.toml](spec/conformance/manifest.toml), all
      three cores in lockstep, pinned by
      [spec/conformance/suites/query/order_by.test](spec/conformance/suites/query/order_by.test).
      _(size: M)_
- [x] **`DISTINCT`.** Done: `SELECT DISTINCT` deduplicates the **projected** output rows
      (NULL-safe — two NULLs collapse, the `IS NOT DISTINCT FROM` rule, not three-valued `=`).
      It runs **after `ORDER BY`, before `LIMIT`/`OFFSET`** (the window slices the *distinct*
      rows), inverting the un-DISTINCT pipeline. Output order is deterministic: first-occurrence
      over the primary-key scan with no `ORDER BY`, else the keys order the distinct rows.
      `ORDER BY` under DISTINCT takes the **PostgreSQL restriction** — each key must be a bare
      column in the select list (or `*`), else the new **`42P10`**
      (`invalid_column_reference`, [registry.toml](spec/errors/registry.toml)); NULL ordering
      follows PostgreSQL too (NULL largest, `ASC` → NULLS LAST). The cost
      asymmetry ([cost.md §3](spec/design/cost.md)) is a cross-core contract: projection
      `operator_eval` is charged per **filtered** row (dedup must evaluate all), `row_produced`
      only per emitted distinct+windowed row, dedup unmetered — so `SELECT DISTINCT 1/a … LIMIT 1`
      traps `22012` where the un-DISTINCT form does not. `DISTINCT` is non-reserved (a column may
      be named `distinct`), disambiguated by a **two-token lookahead** byte-identical across the
      three parsers. Authored in [grammar.ebnf](spec/grammar/grammar.ebnf) (`select`) +
      [grammar.md §11](spec/design/grammar.md), capability `query.distinct` in
      [manifest.toml](spec/conformance/manifest.toml), all three cores in lockstep, pinned by
      [spec/conformance/suites/query/distinct.test](spec/conformance/suites/query/distinct.test).
      _(size: S–M)_
- [x] **Predicate forms** — `IN (list)`, `BETWEEN`, `LIKE`, `CASE`. Done in **four vertical
      slices** (all three cores in lockstep), all edge cases verified against the live `postgres:18`
      oracle. (1) **`IN (list)` / `NOT IN`** and (2) **`BETWEEN` / `NOT BETWEEN`** are non-associative
      postfix forms at the comparison level (a two-token `NOT` lookahead), **desugared at resolve**
      into the existing `=`/`OR`/`AND`/`NOT` nodes — so three-valued NULL (`1 IN (2,NULL)` is NULL;
      the Kleene-AND `5 BETWEEN 10 AND NULL` is FALSE), per-element/bound typing (22003/42804), and
      cost all fall out (the LHS is re-evaluated per element/bound). BETWEEN's bounds parse at the
      additive level so the structural `AND` is not the connective. (3) **`LIKE` / `NOT LIKE`** is a
      genuine catalog operator (text×text→boolean) with a hand-written **code-point** matcher (`%`/`_`,
      default `\` escape) — `'😀x' LIKE '_x'` is TRUE; a pattern ending in a lone escape *reached
      during matching* traps the new **`22025`** lazily (matching PG). (4) **`CASE`** (searched +
      simple) is the engine's first **lazy** expression and the one sanctioned no-short-circuit
      exception (cost.md §3): first-TRUE wins, later arms unevaluated; result arms unify (numeric
      promote, all-NULL → text, cross-family 42804). Authored spec-first: grammar.ebnf + grammar.md
      §20–§23, cost.md §3 (CASE exception), error `22025`, the `like` catalog operator (+ codegen,
      verify.rb), capabilities `expr.{in_list,between,like,case}` + the `predicates` profile; pinned
      by `spec/conformance/suites/expr/{in_list,between,like,case}.test` (50/0/0 byte-identical in
      Rust, Go, TS, `# cost:`/`# names:` asserted). **Deferred narrowings:** `IN (subquery)` (Phase 4
      subqueries); LIKE `ESCAPE 'c'` clause, `ILIKE`, `SIMILAR TO`; CASE integer-arm width follows our
      default (int64) rather than PG's exact promoted width (unobservable — all integers render `I`).
      _(was: M; LIKE deps: text type ✓)_
- [x] **Aggregates** `COUNT` / `SUM` / `MIN` / `MAX` / `AVG` + **`GROUP BY`** + **`HAVING`**.
      Done in **three vertical slices** (all three cores in lockstep): (1) the engine's first
      **function-call syntax** (`name ( * | expr )`, one-token lookahead so names stay
      non-reserved; only aggregates resolve, unknown → `42883`, `DISTINCT`-in-aggregate →
      `42601`) + **whole-table aggregation** (one result row, even over an empty table); (2)
      **`GROUP BY`** (bare/qualified columns, value-canonical bucketing so `1.5`/`1.50` and NULL
      group correctly, first-occurrence order, the **grouping-error rule** `42803`, `ORDER BY`
      over grouping keys); (3) **`HAVING`** (boolean filter over grouped rows, after aggregation
      before ORDER BY, may reference unprojected aggregates). **PostgreSQL widening** (verified
      against the live `postgres:18` oracle): `COUNT`→`int64`; `SUM(int16/int32)`→`int64`,
      `SUM(int64)`→`decimal`, `SUM(decimal)`→`decimal`; `AVG(any numeric)`→`decimal` via the
      exact decimal division; `MIN`/`MAX`→input type; NULL inputs skipped (COUNT(\*) counts
      rows), overflow traps `22003`. New canonical data: a `[[aggregate]]` array
      (`kind = "aggregate"`) in [catalog.toml](spec/functions/catalog.toml) with its own
      verify.rb branch + codegen `AGGREGATES` table; errors `42803`/`42883`; cost unit
      `aggregate_accumulate`; design doc [aggregates.md](spec/design/aggregates.md);
      [grammar.md](spec/design/grammar.md) §17–§19; capabilities `query.aggregates` /
      `query.group_by` / `query.having` + profiles `aggregates`/`grouping`/`having`; conformance
      [suites/aggregates/](spec/conformance/suites/aggregates/) (`count`/`sum`/`min_max`/`avg`/
      `whole_table`/`group_by`/`having`), 46/0/0 byte-identical in Rust, Go, and TS with
      `# cost:` / `# names:` pinned. Deferred: `COUNT(DISTINCT x)`, `SELECT DISTINCT` in an
      aggregate query, GROUP BY by expression/ordinal/alias, the functional-dependency grouping
      relaxation, `GROUPING SETS`/`FILTER`/ordered-set aggregates. _(size: L; deps: expression evaluator)_
- [x] **Scalar functions** `abs` / `round` — the first named per-row functions. Done across
      Rust/Go/TS (conformance 54/54 byte-identical, PG-oracle-verified).
      Authored as `[[operator]]` rows with `kind = "function"` (reusing the operator mold,
      [functions.md](spec/design/functions.md) §9; no symbol/precedence), so codegen +
      verify.rb + the `spec_constants` drift tests accept them unchanged. The shared
      `function_call` grammar generalizes its argument to a **comma-separated list**
      (`abs(x)`, `round(x)`, `round(x, n)`) — the `FuncCall`/`FuncCallExpr` AST node goes from
      a single `arg` to an `args` list across all three cores — and the resolver splits
      aggregate vs scalar vs unknown (`42883`). `abs` → operand type, range-checks at the
      result boundary (`abs(int16 -32768)` → `22003`); `round` → numeric, half-away to scale 0
      or `n`, with explicit integer overloads so PG's `round(5)` works (no implicit coercion).
      Scalar functions are valid **anywhere an expression is** (incl. `WHERE`), unlike
      aggregates; one `operator_eval` per call. Capabilities `func.abs`/`func.round` + profile
      `functions`; conformance
      [suites/expr/scalar_functions.test](spec/conformance/suites/expr/scalar_functions.test).
      No new type / on-disk-format change. Follow-ons: `ceil`/`floor`/`mod`/`sign`, text
      `length`/`lower`/`upper`, a general implicit argument-coercion pass. _(size: M; deps: expression evaluator, decimal)_
- [x] **Multi-row `INSERT`** (`VALUES (..),(..)`). Done: `INSERT INTO t VALUES (..),(..)`
      accepts one or more parenthesized rows, **two-phase / all-or-nothing** like `UPDATE`
      (CLAUDE.md §11 step 6) — every row is fully validated (arity → `42601`, type/range →
      `22003`, NOT NULL → `23502`) and every storage key checked for a duplicate (`23505`,
      against both stored rows **and** earlier rows in the same batch) **before any row is
      inserted**, so a mid-batch failure stores nothing. Synthetic rowids (no-PK tables) are
      allocated in phase two, in row order, so a failed batch burns none. The `Insert` AST
      went from one `values` row to `rows: [][]Literal` across all three cores. Authored in
      [grammar.ebnf](spec/grammar/grammar.ebnf) (`insert` / `row`) +
      [grammar.md §12](spec/design/grammar.md), capability `dml.insert_multi_row` in
      [manifest.toml](spec/conformance/manifest.toml), all three cores in lockstep, pinned by
      [spec/conformance/suites/dml/insert_multi_row.test](spec/conformance/suites/dml/insert_multi_row.test).
      _(size: S)_
- [x] **`INSERT ... SELECT`** — insert the rows a query produces (the second half of the
      original multi-row-INSERT item). Done: the `insert` grammar source is now
      `( VALUES ... | select )` ([grammar.ebnf](spec/grammar/grammar.ebnf),
      [grammar.md](spec/design/grammar.md) §24); the executor feeds the SELECT result set through
      the same two-phase, all-or-nothing validation as VALUES (a shared `insertRows` /
      `insert_rows` helper across all three cores). Two checks run **up front, before any row is
      produced** (so they fire even over an empty source — the full-PG behaviour): output **arity**
      must match the target (`42601`) and each projected column's **type** must be assignable to its
      target (`42804`, the family-level subset of `store_value`, surfaced by threading projection
      types out of `resolveProjections` via an internal `runSelect`/`SelectResult` — the public
      `Outcome` is unchanged). Cost = the embedded SELECT's accrued cost (not the VALUES form's
      zero); the source is materialized first, so a self-insert reads the pre-insert snapshot. New
      capability `dml.insert_select` (in the `constraints` profile); pinned by
      [spec/conformance/suites/dml/insert_select.test](spec/conformance/suites/dml/insert_select.test)
      (52/52 byte-identical across Rust/Go/TS). _(size: M; deps: SELECT ✓)_
- [x] **`DROP TABLE`.** Done: `DROP TABLE t` removes a table — its definition **and** all
      its rows — from the catalog (both the catalog entry and the per-table store, keyed by
      the lower-cased name; case-insensitive). The inverse of `CREATE TABLE`: dropping a
      table that does not exist traps **`42P01`** (`undefined_table`, the same code the DML
      paths raise), mirroring CREATE's `42P07`-on-duplicate. After a drop the name is free to
      re-create from empty. Cost is **zero** (a pure catalog edit — no rows read, no
      expression tree, the store discarded wholesale). Deliberate narrowings, each relaxable
      later: **no `IF EXISTS`** (kept symmetric with the still-missing `CREATE TABLE IF NOT
      EXISTS`), **single table** (no `DROP TABLE a, b`), and **no `CASCADE`/`RESTRICT`** (no
      dependent objects exist yet). Authored in [grammar.ebnf](spec/grammar/grammar.ebnf)
      (`sql_statement` / `drop_table`) + [grammar.md §13](spec/design/grammar.md), capability
      `ddl.drop_table` in [manifest.toml](spec/conformance/manifest.toml), all three cores in
      lockstep, pinned by
      [spec/conformance/suites/ddl/drop_table.test](spec/conformance/suites/ddl/drop_table.test)
      (the first `ddl/` suite). _(size: S)_

---

## Phase 3 — The type system as the product (the differentiator, §4)

> The **real type system** is the product (§4) — PostgreSQL's behavior, stricter than its
> typing, and nothing like SQLite's runtime affinity. Each item is a vertical slice that
> forces a §8 divergence decision into the open (default: match PG — §1). `text` (collation
> `C`), `decimal` (exact base-10, half-away rounding), `bytea` (unsigned byte order, `\x`-hex
> literals), `uuid` (fixed 16 bytes, PG-flexible input, and the **first non-integer `PRIMARY
> KEY`**), and `timestamp`/`timestamptz` (int64-µs instant model, no tz database) are all done;
> `json`/`array` are the remaining headline items.

- [x] **Storable `boolean` column type** — done & committed across Rust/Go/TS. `boolean` was
      expression-only (Phase 1); it is now a *column* type: `CREATE TABLE t(flag boolean)`,
      `INSERT`/store/retrieve of `false`/`true`/`NULL`, `boolean × boolean` comparison
      (`= < > <= >=`, `IS [NOT] DISTINCT FROM`) and `ORDER BY` (false `<` true, NULLs last).
      On-disk type code `5` (codes 1–4 are int16/int32/int64/**text**) with the 1-byte `bool-byte`
      value codec, byte-exact across cores (golden `bool_table.jed`); capability
      `types.boolean_storable`; corpus `spec/conformance/suites/types/boolean.test`. Cleanly
      additive (old files keep working). Two deliberate narrowings remain (below). _(size: M;
      §4/§8/§9)_
  - [ ] **boolean in a key / `PRIMARY KEY`** — rejected `0A000` this slice; the order-preserving
        `bool-byte` key rule is authored (`scalars.toml`) but unexercised. Lifting it adds the
        executor key path + `bool-byte` key-encoding byte-vectors. _(size: S)_
  - [ ] **boolean⇄integer casts** — `CAST(x AS boolean)` / `CAST(bool AS int)` rejected
        (`0A000` / `42804`); not in the cast matrix. PostgreSQL's are asymmetric (bool→int yes,
        int→bool no), so authored in a dedicated cast slice, not here. _(size: S; §5)_
- [x] **`text` + ONE defined collation** — done & committed across Rust/Go/TS. Collation is
      PostgreSQL `C` (UTF-8 byte / code-point order; `scalars.toml` records the type with
      `collation = "C"`). Storage + single-quoted literals (`''` escaping) + comparison/ordering
      (`= < > <= >=`, `IS [NOT] DISTINCT FROM`); on-disk type code 4 with a compact value codec
      (u16 len + UTF-8 bytes), byte-exact across cores (golden `text_table.jed`). First operator
      **overload** (`=` over integer & text) — `catalog.toml` carries one row per `(name,
      arg_families)`; `functions/verify.rb` and the per-core drift tests key on the signature.
      The UTF-8-vs-UTF-16 ordering trap is handled in TS (`compareTextC` encodes to UTF-8, never
      JS `<`) and pinned by an astral-char conformance case. _(was: L; §4/§8; spec/design/types.md §11)_
      **Deferred follow-ups:** text in a `PRIMARY KEY` / index (the order-preserving
      terminator+escape key encoding is authored in `encoding.md §2.4` but unexercised — text PK
      is rejected `0A000`); `varchar(n)` length limits (`22001`); text⇄other casts; string
      functions (`||`, `length`, `lower`/`upper`, `substring`) + `LIKE`; multi-collation / ICU
      (a per-column catalog collation field + `COLLATE`).
- [x] **Exact `decimal`** — *the* headline type. Done across Rust/Go/TS: an exact base-10
      numeric held as hand-rolled sign + base-10⁹ coefficient + scale (no bignum lib, no float),
      the engine's **first parameterized type** (`numeric`, `numeric(p)`, `numeric(p,s)`;
      `1≤p≤1000`, `0≤s≤p`, bad typmod `22023`). Settles the §8 **decimal-rounding** hotspot:
      **round half away from zero** (PG `numeric`), one mode engine-wide, with PG-faithful
      result **scales** (add/sub `max(s1,s2)`, mul `s1+s2`, div `select_div_scale`, mod
      `max(s1,s2)`). Comparison/order by exact value (`1.5 = 1.50`), the first cross-family
      `integer↔decimal` promotion, casts (`int→decimal` implicit, `decimal→int` explicit-only —
      stricter than PG), arithmetic `+ − * / %` + unary `−`, on-disk value codec (type code 5,
      base-10⁴ groups), render tag `D`. **Finite only** — no NaN/±Infinity (documented PG
      divergence). Authored in [spec/design/decimal.md](spec/design/decimal.md) + the type/
      function/error/grammar data, capabilities `types.decimal` + `expr.decimal_arithmetic`,
      pinned by `spec/conformance/suites/types/decimal.test`,
      `spec/conformance/suites/expr/decimal_arithmetic.test`, and the byte-exact golden
      `spec/fileformat/fixtures/decimal_table.jed` (read/written identically by all three cores +
      the Ruby reference). `numeric.c` (Postgres) was the reference. _(was: XL; §4/§8)_
      **Deferred follow-ups:** decimal in a `PRIMARY KEY`/index (the order-preserving
      `decimal-order-preserving` key encoding is authored in `encoding.md §2.5` but unexercised
      — decimal PK is rejected `0A000`); scientific `e`-notation literals (`1.5e3`); negative /
      `s>p` scale typmods (PG 15+); `round(x,n)` and other decimal functions; raising the
      1000-digit / scale-1000 cap once over-page values land (overflow pages / TOAST —
      [spec/design/large-values.md](spec/design/large-values.md), Phase 6).
- [x] **`timestamp` / `timestamptz`** — done & committed across Rust/Go/TS (`1ee7027`). The
      PostgreSQL **instant** model (not the SQL-standard offset-bearing one): `timestamp` is a
      zoneless wall clock, `timestamptz` a UTC instant whose input offset normalizes to UTC then
      is **discarded**. Both are **int64 microseconds** since the Unix epoch (proleptic Gregorian,
      no leap seconds) — two distinct types sharing one physical representation (on-disk type codes
      **9** / **10**; they never compare or cast to each other → `42804`). Deliberately **no
      time-zone database / named zones** — kept deterministic + dependency-free (§8/§14, no
      wall-clock in tests); named-zone handling is left to the host. Calendar math is Hinnant
      `days_from_civil` / `civil_from_days`, authored once in
      [spec/design/timestamp.md](spec/design/timestamp.md) and transcribed identically into all
      three cores (the §8 determinism hotspot: civil↔days truncating, instant↔civil floor).
      `infinity` / `-infinity` are first-class (`i64::MIN`/`MAX` sentinels, totally ordered), so
      ordering, key encoding, and the on-disk codec handle them for free; **timestamp/timestamptz
      `PRIMARY KEY`** is supported (reuses the int64 order-preserving key codec). New errors
      `22007` / `22008`; capabilities `types.timestamp` / `types.timestamptz` + the `timestamps`
      profile; pinned by `spec/conformance/suites/types/{timestamp,timestamptz}.test` (38/0/0
      byte-identical in Rust, Go, TS) and the byte-exact goldens
      `{timestamp,timestamptz}_table.jed` (rust==go==ts==ruby). Oracle-verified vs PG 18.3 (all
      epoch values + renders match). **Two documented divergences (by design):** sub-µs rounding is
      **half-away** (jed's one rounding mode, no float in the value path) vs PG's half-even; a `:60`
      seconds field is **rejected** (strict) vs PG's roll-to-next-minute. _(was: L; §4;
      spec/design/timestamp.md, encoding/timestamps.toml)_ **Deferred follow-ups:** an `interval`
      type + timestamp arithmetic; date/time functions (`now()`/`current_timestamp`, `EXTRACT`,
      `date_trunc`, `age`); separate `date` / `time` types; named-zone `AT TIME ZONE` (needs the
      host-supplied tz database); timestamp⇄text/date casts; sub-second precision typmods
      (`timestamp(p)`).
- [x] **`bytea`** — done & committed across Rust/Go/TS. A variable-width binary string (raw
      bytes), compared by **unsigned byte order** (PostgreSQL's bytea comparison). Storage +
      `\x`-hex literals + comparison/ordering (`= < > <= >=`, `IS [NOT] DISTINCT FROM`); on-disk
      type code 7 with the same compact value codec as text (u16 len + raw bytes, no UTF-8
      validation), byte-exact across cores (golden `bytea_table.jed`). Another comparison
      operator **overload** (catalog.toml carries `bytea`-family rows). A bytea literal is a
      single-quoted string that **adapts to a bytea context** (the integer-literal
      context-adaptation rule of §6 extended to strings — `INSERT INTO t VALUES (1, '\xff')`,
      `WHERE b = '\xab'`; no cast needed); **hex input only** (`\x` + even hex digits), malformed
      hex traps **`22P02`** deterministically pre-scan; rendered `\x`+lowercase-hex. Unlike text
      there is no UTF-16 ordering trap (bytea is raw bytes). _(was: M; §4/§8; spec/design/types.md
      §13, encoding.md §2.6)_ **Deferred follow-ups:** bytea in a `PRIMARY KEY` / index (the
      order-preserving `bytea-terminated-escape` key encoding is authored in `encoding.md §2.6`
      but unexercised — bytea PK is rejected `0A000`); the traditional escape input format
      (`\nnn`); bytea⇄other casts; binary functions (`length`, `||`, `substring`,
      `encode`/`decode`, `get_byte`).
- [x] **`uuid`** — done & committed across Rust/Go/TS. A fixed **16-byte** value (RFC 4122),
      compared by **unsigned byte order** over the 16 bytes. Storage + comparison/ordering
      (`= < > <= >=`, `IS [NOT] DISTINCT FROM`); on-disk type code **8** with the engine's first
      **fixed-width non-integer** value codec (16 raw bytes, **no** length prefix), byte-exact
      across cores (golden `uuid_table.jed`). Another comparison-operator **overload**
      (`catalog.toml` carries `uuid`-family rows). A uuid literal is a single-quoted string that
      **adapts to a uuid context** (the §6 string-adaptation rule, like bytea), with
      **PostgreSQL-flexible input** replicating `uuid_in` (optional `{}`, any case, an optional
      hyphen after each whole byte-pair — canonical `8-4-4-4-12`, hyphen-less 32-hex, and the
      every-4-digit grouping all accepted; a misplaced hyphen is rejected), normalized to the
      canonical **lowercase** `8-4-4-4-12` on **output**; malformed input traps **`22P02`**
      pre-scan. Rendered under the `T` tag. **First non-integer `PRIMARY KEY`** — uuid lifts the
      key narrowing the other non-integer types defer: its `uuid-raw16` order-preserving key
      encoding (bare 16 bytes — no escape/terminator/sign-flip) is **exercised** (CREATE/INSERT/
      point-lookup/`ORDER BY`/duplicate-key over a uuid PK), proving the executor key path
      generalizes beyond integers. Authored spec-first: `spec/design/types.md §14`,
      `encoding.md §2.7` (+ uuid key vectors in `encoding/integers.toml`), `format.md` (type code
      8 + value codec), `catalog.toml` (+ codegen), capability `types.uuid`; pinned by
      `spec/conformance/suites/types/uuid.test` (51/0/0 byte-identical in Rust, Go, TS, with
      `# cost:` asserted) and the byte-exact golden `uuid_table.jed`. _(was: M; §4/§8;
      spec/design/types.md §14, encoding.md §2.7)_ **Deferred follow-ups:** uuid⇄other casts
      (`text ⇄ uuid`, `bytea ⇄ uuid` — rejected `0A000`/`42804`, a later cast slice); uuid
      functions (`gen_random_uuid()`, `uuid_generate_v*`).
- [ ] **`json` / `jsonb`** — optional headline feature (§1). Large surface. _(size: XL; §4)_
- [ ] **Composite `array` type** — a **container** over the scalar set: a new type *axis*,
      not another scalar (CLAUDE.md §4). Array literals, element-type rules, `NULL` element
      vs `NULL` array, equality/ordering, and an order-preserving key encoding for
      arrays-in-keys. Match PostgreSQL array semantics by default (§1). Large surface;
      sequence after the core scalar set settles. _(size: XL; §4/§8)_
- [ ] **Float policy decision.** §8 deliberately keeps `f64` out of compare/text-output
      paths. Decide if floats ever exist, and if so how rendered. _(size: S decision / L if built; §8)_

---

## Phase 4 — Relational depth + constraints

> The meaty planner/executor work and the rest of the integrity story.

- [x] **`JOIN` — multi-table FROM + `INNER`/`CROSS`** — done & committed across Rust/Go/TS. The
      `SELECT` FROM clause grew from a single table name to a **left-deep chain**
      (`from_clause ::= table_ref join_clause*`): **table aliases** (`t AS a` / `t a`), **qualified
      column references** (`t.col`, via a new `Dot` token), a **scope resolver** (an ordered list
      of `(label, table, column-offset)` that bakes a flat index into the existing `Column` node —
      so the joined row is each relation's row **concatenated** and the whole expression evaluator
      is untouched), and a **left-deep nested-loop** executor. Bare column ambiguous across
      relations → **`42702`** (`ambiguous_column`, new), unknown qualifier → `42P01`, self-join
      without distinct aliases → **`42712`** (`duplicate_alias`, new), non-boolean `ON` → `42804`.
      The `ON` is three-valued (a NULL join key never matches) and evaluated **at its join node**
      (not folded into WHERE), so outer joins are a clean executor-only follow-on. Cost is the
      cross-core contract ([cost.md §3](spec/design/cost.md)): `storage_row_read` per materialized
      row (Σ cardinalities), `operator_eval` per `ON` candidate combination, `row_produced` per
      emitted row. Authored in [grammar.ebnf](spec/grammar/grammar.ebnf) + [grammar.md §15](spec/design/grammar.md),
      capabilities `query.join_inner` / `query.cross_join` / `query.table_alias` /
      `query.qualified_column` + the `joins` profile in [manifest.toml](spec/conformance/manifest.toml),
      pinned by `spec/conformance/suites/joins/*.test`. _(was: L; deps: expression evaluator)_
  - [x] **Outer joins — `LEFT`/`RIGHT`/`FULL [OUTER] JOIN`** — done & committed across Rust/Go/TS.
        **Executor-only** follow-on as planned: the existing left-deep nested-loop gained an
        "unmatched row → NULL-extend the absent side" branch (LEFT/FULL preserve unmatched left rows,
        RIGHT/FULL preserve unmatched right rows), with NULL-pad widths taken from the **scope** (not a
        sampled row, so an empty intermediate result pads correctly). The three-valued `ON` is unchanged
        (a NULL key NULL-extends rather than drops), `WHERE` still runs post-join (the PG "WHERE on the
        nullable side downgrades to inner" behavior falls out for free), and cost matches the inner join
        except for the extra preserved rows — NULL-extension charges no `operator_eval`
        ([cost.md §3](spec/design/cost.md)). New capabilities `query.join_left` / `query.join_right` /
        `query.join_full` + the `outer_joins` profile in [manifest.toml](spec/conformance/manifest.toml),
        pinned by `spec/conformance/suites/joins/{left,right,full}.test`; semantics documented in
        [grammar.md §15](spec/design/grammar.md). `USING` / `NATURAL` / comma-`FROM` / `t.*` stay
        deferred. _(was: M; deps: INNER/CROSS slice)_
- [x] **Subqueries (uncorrelated)** — done & committed across Rust/Go/TS: a **scalar**
      `(SELECT …)` in expression position, `x [NOT] IN (SELECT …)`, and `[NOT] EXISTS (SELECT …)`.
      The key move is **plan-time folding** ([grammar.md §26](spec/design/grammar.md)): because an
      uncorrelated subquery's result is independent of any outer row, a **pre-pass at the top of
      `run_select`** (before scope/resolution, where the db is already in hand) executes each
      subquery **exactly once** and replaces it with a constant the ordinary resolver/evaluator
      already handle — **the per-row expression evaluator is untouched** (the whole reason the slice
      is small, and the seam the correlated half will extend). Fold rules: scalar → a `FoldedConst`
      carrying the value **and its output type** (so it promotes/compares like that type; 0 rows → a
      **typed** NULL, >1 row → **`21000`** cardinality_violation [new error, class 21], >1 col →
      `42601`); EXISTS → a boolean literal `(rows>0)` (select list ignored, never NULL); IN → the
      literal-`IN` OR-chain over the result values (3VL inherited verbatim), an **empty** result →
      an empty `In` that resolves to constant FALSE/TRUE. **Cost** = the enclosing query's cost **+**
      each subquery's cost counted **once** (the folded constant is a leaf — no `operator_eval`;
      mirrors the set-op / `INSERT … SELECT` precedent, [cost.md §3](spec/design/cost.md)). New
      capabilities `query.subquery_scalar` / `query.subquery_in` / `query.subquery_exists` + the
      `subqueries` profile in [manifest.toml](spec/conformance/manifest.toml), pinned by
      `spec/conformance/suites/subquery/{scalar,in,exists,errors}.test` (64/0/0 all cores,
      byte-identical incl. cost). Semantics verified against the live `postgres:18` oracle.
      **Deferred narrowings (each → `0A000`, relaxable):** a **correlated** reference (now landed —
      see below); a **bind parameter `$N` inside** a subquery; subqueries are **SELECT-only** (one in
      an UPDATE/INSERT/DELETE expression is `0A000`). _(was: L; deps: joins)_
  - [x] **Correlated subqueries** — done & committed across Rust/Go/TS (the **principled, multi-level**
        slice). `run_select` was **split into a resolve phase (`plan_query`) and an execute phase
        (`exec_query_plan`)** so a subquery is resolved **once** into an owned plan — its column-count /
        type errors fire even over an **empty** outer (PG parity) — yet **re-executed per outer row**.
        Resolution gained a **scope chain**: `Scope` carries a `parent` + the catalog, and
        `resolve_bare`/`resolve_qualified` walk outward, returning `Local(idx)` or `Outer{level,index}`
        (a correlated ref → an `OuterColumn` leaf; **any** depth — parent, grandparent, …; nearest
        scope shadows). The per-row evaluator now takes an **`EvalEnv`** (the engine + bound params +
        the stack of enclosing rows): an `OuterColumn` reads the stack, and a surviving (correlated)
        `Subquery` node pushes the current row and runs its inner plan. A post-bind **`fold_uncorrelated`
        pass** keeps a globally-uncorrelated subquery (a PG "initplan") folded **once** (an uncorrelated
        `IN` → an `InValues` node), so the committed once-only cost is unchanged. **Cost** (cost.md §3):
        a correlated subquery adds one `operator_eval` + its inner plan's cost **per outer row** it
        evaluates; deterministic + byte-identical cross-core (pinned `# cost:` in
        `spec/conformance/suites/subquery/correlated.test`, 65/0/0 all cores). New capability
        `query.subquery_correlated`. Outer refs work in WHERE / HAVING / select-list / aggregate args /
        a nested JOIN `ON`. **Remaining narrowing (→ `0A000`):** a **correlated `GROUP BY` /
        `ORDER BY` key** (degenerate). (Two narrowings here are now lifted: subqueries were
        **SELECT-only** — see UPDATE/DELETE below — and a **`$N` inside** was rejected — see $N below.)
        A pure-outer aggregate arg (`sum(outer.col)`) is a documented
        divergence (jed sums at the inner level; PG binds it to the outer query — grammar.md §26).
        Semantics verified against the live `postgres:18` oracle. _(was: L)_
  - [x] **Subqueries in UPDATE / DELETE** — done & committed across Rust/Go/TS. A subquery is now
        legal in a `DELETE`/`UPDATE` `WHERE` and an `UPDATE` assignment RHS (the **SELECT-only**
        narrowing above, lifted). The machinery was already in place from the correlated slice:
        `Scope::single` (the one-relation UPDATE/DELETE scope) flips `allow_subquery` **true**, and the
        mutation paths run the **`fold_uncorrelated` pass** over the resolved WHERE / assignment RHSs
        before the scan, then build a real per-row `EvalEnv` (the engine + bound params). An
        **uncorrelated** subquery folds once (cost added once); a **correlated** one names the **target
        row** (its parent is the single scope, so `t.col` → `OuterColumn{level 1}`) and re-runs per
        **scanned** row, reading the OLD row. Two-phase / all-or-nothing is preserved: the subquery sees
        the **pre-statement snapshot** (DELETE collects keys before removing; UPDATE validates all before
        writing). **Cost** (cost.md §3): same as the SELECT case — pinned `# cost:` in
        `spec/conformance/suites/subquery/mutation.test` (66/0/0 all cores). No new capability (reuses
        `query.subquery_*` + `dml.delete`/`dml.update`). Semantics verified against the live `postgres:18`
        oracle. _(was: part of M)_
  - [x] **`$N` inside a subquery** — done & committed across Rust/Go/TS. The `plan_subquery` guard
        that rejected any bind parameter inside a subquery (`0A000`) is gone — the original blocker
        (per-`run_select` param inference) was already removed by the correlated slice, which threads
        **one** `ParamTypes` through the whole plan tree. So a `$N` typed by an **inner** context
        (`WHERE inner.col = $1`, `… IN (SELECT … WHERE x = $1)`) infers statement-wide, the **same**
        `$N` can appear inside and outside the subquery (the uses unify), and a correlated subquery may
        compare a `$N` against the outer row. The lone gap: a `$N` whose **only** type context is the
        *enclosing* query (`k = (SELECT $1 …)`) would need **bidirectional** inference into the
        subquery — jed doesn't, so it stays uninferred and `finalize` raises **`42P18`**. Documented
        divergence (PG defaults such a `$N` to `text` → `42883`); jed's `42P18` names the real cause
        and fits its strict, no-guessing type system (CLAUDE.md §4). Dead `expr_has_param`/
        `query_has_param`/clause-walk helpers removed in all three cores. No new capability; corpus
        `subquery/errors.test` now pins the `42P18` (uninferable) + `42601` (inner-typed, no value)
        cases. Semantics verified against the live `postgres:18` oracle. _(was: part of M)_
  - [ ] **Subqueries — remaining seams:** subqueries in an **`INSERT ... VALUES`** slot (blocked on
        VALUES holding a general expression — a separate narrowing; `INSERT ... SELECT` already admits
        them); **derived tables** (`FROM (SELECT …) AS t`); **`ANY` / `ALL`** and row-valued subqueries.
        _(size: M)_
- [x] **Set operations** — `UNION [ALL]`, `INTERSECT [ALL]`, `EXCEPT [ALL]` — done & committed across
      Rust/Go/TS. The top-level query grew from a single `select` to a **query expression**
      (`query_expr ::= set_expr order_by? limit_offset?`): a two-level precedence tree
      (`INTERSECT` binds **tighter** than `UNION`/`EXCEPT`, which are equal-precedence and
      left-associative — the PostgreSQL precedence) over `select_core`s (a SELECT with **no**
      trailing ORDER BY/LIMIT/OFFSET, which hoist to the whole result). AST is **additive** —
      `Statement::SetOp` + a recursive `QueryExpr { Select | SetOp }`; a lone SELECT stays
      `Statement::Select`, so the plain-query path and host API are byte-unchanged. The set
      operators were added to the **table-ref stop-keyword** set so `FROM a UNION …` is not
      swallowed as an implicit alias. Per-column **type unification** is full-PG: integer width
      promotion, integer↔decimal → decimal (the narrower operand's **values are converted before
      row-keying** — load-bearing so `1 INTERSECT 1.0` matches), all-NULL → text; output **column
      count + names come from the left operand**. Row identity is **NULL-safe + value-canonical**
      (reusing the DISTINCT key machinery), with multiset semantics `min(m,n)` / `max(0,m−n)` for
      the `ALL` variants and the emitted representative = first occurrence (left scanned first). A
      trailing `ORDER BY` resolves keys by **output column name** (qualified key → `42P01`, unknown
      → `42703`; ordinals stay deferred). Arity mismatch → **`42601`**, type mismatch → **`42804`**
      (no new error codes). Cost is the cross-core contract ([cost.md §3](spec/design/cost.md)):
      **`lhs.cost + rhs.cost`** — the combine/dedup, the trailing sort, and the LIMIT/OFFSET window
      are unmetered (mirrors `INSERT … SELECT`), so a LIMIT does **not** lower the cost. Semantics
      pinned against the live `postgres:18` oracle. Authored in
      [grammar.ebnf](spec/grammar/grammar.ebnf) + [grammar.md §25](spec/design/grammar.md),
      [types.md §4](spec/design/types.md), capabilities `query.union` / `query.intersect` /
      `query.except` + the `set_operations` profile in [manifest.toml](spec/conformance/manifest.toml),
      pinned by `spec/conformance/suites/setops/*.test` (60/0/0 all cores). **Deferred narrowings**
      (relaxable later): no parenthesized operands `(SELECT …) UNION …`, no ORDER BY/LIMIT inside an
      operand (→ `42601`), no ORDER BY ordinals, and no set operation in an `INSERT … SELECT` source.
      _(was: M)_
- [x] **`NOT NULL`** — explicit column constraint; storing NULL (direct, omitted, or applied
      default) traps `23502`. PRIMARY KEY still implies it (spec/design/constraints.md §1).
- [x] **`DEFAULT`** (literal) — `DEFAULT <literal>` column constraint, evaluated + coerced once
      at CREATE TABLE; applied for an omitted column or the `DEFAULT` keyword; persisted via flags
      bit2 + the value codec. Landed with the **`INSERT` column list** + the `DEFAULT` value
      keyword (grammar.md §16, constraints.md §2). A general-expression default stays deferred.
- [ ] **Constraints (remaining)** — `UNIQUE`, `CHECK`, **composite `PRIMARY KEY`** (key encoding
      already composes — types.md §7), `FOREIGN KEY`. These are heavier. _(size: M→L each)_
- [ ] **Secondary indexes** (`CREATE INDEX`) — also a planner + storage concern (index
      pages, index maintenance on write). _(size: L; deps: storage maturation)_
- [ ] **`RETURNING`** clause; **`UPSERT` / `ON CONFLICT`**. _(size: M; deps: UNIQUE)_
- [ ] **Relax the UPDATE narrowings** — allow assigning a `PRIMARY KEY` column (currently
      `0A000`; means the storage key can change). Documented as relaxable (§11 step 6).
      _(size: M; deps: transactions for clean re-keying)_

---

## Phase 5 — Transactions & the §3 commit model

> The real concurrency story. ✅ **Phase 5 is landed (P5.0–P5.3, all three cores):** autocommit
> + multi-statement `BEGIN`/`COMMIT`/`ROLLBACK` + the immutable-`Snapshot` / working-root commit
> model + the shared handle (concurrent readers + a single writer) + the oldest-live-txid
> watermark. Couples tightly with Phase 6 (the staging buffer *is* the in-memory pending set the
> COW commit flushes); the watermark registry is the free-list gate Phase 6 will consult.
>
> **Design landed** ([spec/design/transactions.md](spec/design/transactions.md)): the model
> is immutable **`Snapshot`**s + a writer's **working root**, unifying the staging area, the
> read snapshot, and the pending set into one structure. The committed store becomes a
> **persistent (copy-on-write) ordered B-tree** (decision **B1**) — chosen as the in-memory
> precursor of the Phase-6 on-disk B-tree, so Phase 6 page-backs the tree rather than building
> one. **jed adopts PostgreSQL autocommit** (correcting the accidental "no autocommit" policy,
> which fell out of the whole-image writer) and **decouples the commit boundary from
> durability** via a **`synchronous`** setting (default on; off batches the fsync). The host
> declares a transaction's **access mode** — `BEGIN [READ ONLY|READ WRITE]` (SQL) or
> `db.begin(writable)` / `db.view`/`db.update` (API); autocommit infers it from the statement
> kind. Ships **fully durable + §3-correct on whole-image commit**; only on-disk *efficiency*
> is deferred to Phase 6.

- [x] **P5.0 — transaction model spec** — authored
      [spec/design/transactions.md](spec/design/transactions.md) (snapshot/working-root model,
      persistent-tree primitive, **autocommit + `synchronous` durability decoupling**,
      **read-only vs read-write access modes** + the `Transaction`/`view`/`update` surface,
      isolation, abort-on-error, the reader-liveness watermark, SAVEPOINT/nested
      non-foreclosure); reconciled [storage.md §4](spec/design/storage.md),
      [api.md](spec/design/api.md) (autocommit replaces "no autocommit"; `close` no longer drops
      committed work; `begin`/`view`/`update`/`synchronous` added), and [CLAUDE.md §9](CLAUDE.md)
      (durability decoupled from the commit boundary); registered class-25 errors **`25001`** /
      **`25006`** / **`25P02`** in [registry.toml](spec/errors/registry.toml). _(size: S)_
- [x] **P5.1 — persistent ordered map + the snapshot refactor (no new SQL).** ✅ Done across
      Rust/Go/TS (`ad68e54`/`4cd7778`/`3c2f3a0`). New `pmap.{rs,go,ts}`: a **copy-on-write
      B-tree** (B1) whose O(1) clone is an independent, structurally-shared snapshot (insert
      splits, delete rebalances — Cormen; unit-tested vs a reference map + a snapshot-independence
      test). `TableStore` wraps it and is an O(1) clone, its API unchanged so the
      executor/format/file are untouched. **Autocommit** (transactions.md §4.1): the statement
      dispatcher captures the committed state cheaply, runs, and on success persists durably
      through the **single `persist` chokepoint** (synchronous=on; TS injects it as a
      `persistHook` storage seam), restoring on any error (rollback-on-error, incl. rolled-back
      rowid allocations §7). `commit`/`rollback` are lenient no-op successes (§4.2); `close` no
      longer drops committed work. Corpus stays green (66/0/0 all cores) + `rake verify` /
      `fmt:check` clean. **Two pieces shifted to where they're first exercised:** the explicit
      `working`-snapshot object lands with P5.2 (multi-statement blocks); the oldest-live-txid
      **watermark** lands with P5.3 (concurrent read snapshots — until then it is trivially the
      committed txid, with no reader to track and no page reclamation to gate). _(size: L; §3; B1)_
- [x] **P5.2 — explicit transactions: SQL `BEGIN`/`COMMIT`/`ROLLBACK` + the `Transaction` API.** ✅
      Done across Rust/Go/TS. **SQL surface** ([grammar.ebnf](spec/grammar/grammar.ebnf) +
      [grammar.md §27](spec/design/grammar.md)): `BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE]` /
      `START TRANSACTION …` open a block (default **READ WRITE**); `COMMIT`/`END [TRANSACTION|WORK]`
      publish; `ROLLBACK [TRANSACTION|WORK]` discard — all keywords non-reserved. New AST
      `Begin{writable}`/`Commit`/`Rollback`; the executor gained a **current-transaction state
      machine** layered on the P5.1 autocommit path: BEGIN captures the committed state (a READ
      WRITE block only — O(1) store clones + shallow catalog), statements run against the **working
      set in place** (read-your-writes; **no** per-statement durable write — the block publishes
      once at COMMIT via the single `persist` chokepoint), and ROLLBACK restores the captured state
      (DDL `CREATE`/`DROP` included, plus rolled-back rowid allocations §7). **Errors** (class 25,
      already registered P5.0): nested `BEGIN` → **`25001`**; a write in a READ ONLY block →
      **`25006`**; any statement error **aborts the block** (failed state) so every later statement
      but `ROLLBACK`/`COMMIT` is **`25P02`**, and `COMMIT` of a failed block acts as `ROLLBACK`
      (PostgreSQL) — discarding earlier successes too. `COMMIT`/`ROLLBACK` with no open block are
      **lenient no-op successes** (no `25P01` — a documented PG divergence, §4.2). **Host API**
      ([api.md §6](spec/design/api.md)): `db.begin(writable)` → `Transaction` (`execute`/`query`/
      `commit`/`rollback`), the bbolt-style closure wrappers `db.view`/`db.update` (auto-commit on
      success / auto-rollback on error or panic), and `db.commit`/`db.rollback`/`close` all drive
      the **same** mechanism (`close` now rolls back an open block). Shared corpus
      [suites/transactions/](spec/conformance/suites/transactions/) (commit/rollback/read_only/
      nested/failed/syntax — visibility is deterministic + single-handle, the harness runs a whole
      file against one handle) + the `transactions` profile / `txn.{explicit,read_only,failed_state}`
      capabilities; the programmatic surface is pinned **per-core** (the corpus is SQL-only). 72/0/0
      byte-identical all cores; `rake verify` + `fmt:check` clean. **The explicit `working`-snapshot
      object landed here** (the per-block captured base + the `tables`/`stores` working set). _(size:
      L; deps: P5.1)_
- [x] **P5.3 — reader/writer concurrency + the watermark.** ✅ Done across Rust/Go/TS, split into
      two sub-slices.
  - **P5.3a — immutable-`Snapshot` foundation.** Refactored the handle into an immutable
    `Snapshot{txid, tables, stores}` + a `Database{committed, tx}` split: reads run against a
    chosen snapshot (`read_snap`/`working` delegation, so the planner/executor were untouched),
    commit swaps `committed := working`, rollback drops `working`, and **every** write — autocommit
    included — runs as a transaction. The persistent (CoW) stores make `committed.clone()` an O(1)
    structurally-shared snapshot. The `oldest_live_txid` **seam** landed here (trivially the
    committed version single-handle). 72/0/0 all cores.
  - **P5.3b — the shared handle (concurrency + the watermark registry).** A separate **`SharedDb`**
    handle (Rust/Go/TS) realizes the §3 model for **concurrent readers + a single writer**: a
    **committed cell** (Rust `RwLock<Arc<Snapshot>>`, Go `atomic.Pointer[Snapshot]`, TS a field), a
    **single-writer gate** (Rust `Mutex<bool>`+`Condvar` / Go held `sync.Mutex` — both **block** a
    second writer; TS **rejects** it `25001`, no thread to block), and the **live-reader registry**
    (`version → refcount`; its minimum is `oldest_live_txid`, the Phase-6 free-list gate, §8).
    `db.read() -> ReadHandle` pins the committed snapshot, serves reads from that immutable version
    (a write through it → `25006`), and deregisters on drop/`close`; `db.write() -> WriteHandle`
    captures a private working set and `commit` publishes it at the next version. **Per-core
    reality** (CLAUDE.md §2 — best experience per language): Rust/Go give **true OS-thread
    parallelism** (reader threads run while a writer commits; Go tested under `-race`), TS gives
    snapshot **isolation** across async interleavings (no shared-memory threads). In-memory this
    slice; file-backed sharing reuses the §9 persist chokepoint, wired later. **Stdlib-only** sync
    primitives (no new deps). The concurrency mechanism is tested **per-core** (not the corpus —
    scheduling/interleaving isn't cross-core deterministic, like `$N`): Rust 7 tests incl. a
    multi-thread reader/writer fan-out, Go 7 under `-race`, TS 6 isolation/watermark tests. Spec:
    [transactions.md §8/§10](spec/design/transactions.md) (watermark registry + the realized
    concurrency mechanism + the per-core split) and [api.md §2.5](spec/design/api.md) (the shared-
    handle surface). 72/0/0 byte-identical all cores; `rake verify` + per-core fmt/vet/typecheck
    clean. _(size: L; §3; deps: P5.2)_

---

## Phase 6 — Storage maturation (§9)

> Can lag the feature work until write volume makes whole-image rewrites costly. The
> forward-compatible hooks (two meta slots, checksum, root pointer, write-ordering) are
> already in place.
>
> **TB-scale non-foreclosure (CLAUDE.md §9):** these items are also the path to a
> **larger-than-RAM file that does not fall over**. RAM-sized is the dominant case but not a
> hard limit — present work must not foreclose >>RAM operation (no full-residency assumption
> above the storage seam; no operator that requires its whole input/output in RAM).
>
> **B1 collapses two XL items into one (transactions.md §3/§9).** Because Phase 5's committed
> store is already a copy-on-write **B-tree** in memory, "incremental COW commit" and "B-tree
> interior pages" below are **one slice (P6.1)**: page-back the existing tree — persisting only
> its dirty nodes to free slots + a meta-root swap *is* the incremental commit. Lands behind a
> **frozen** transaction API. The on-disk B-tree node layout/split rules become a **new §8 byte
> contract** (golden fixtures required) — they are a private in-RAM detail in Phase 5.

- [x] **P6.1 — incremental COW commit = page-backed B-tree** ✅ _(merges ex "incremental COW
      commit" + "B-tree interior pages")_ — replaced the whole-image serialize with
      dirty-page-only writes + meta-slot root swap, the in-memory CoW B-tree persisted
      node-for-page (§9, storage.md §4/§6, transactions.md §3/§9). Landed across all three cores +
      the Ruby reference in two parts. **Part A — the on-disk byte contract** (a §8 hotspot, a
      **clean break to format_version 2**): per-table page-backed B-tree (leaf + interior node
      pages, `page_type` 2/3), relocatable catalog chain, **size-driven fan-out** (`RECORD_MAX =
      (page_size−12−12)/2` so every overflow splits cleanly 2-way), full delete-rebalance
      (merge-then-maybe-split). All 15 golden fixtures regenerated and byte-exact across
      `rust == go == ts == ruby`; the from-scratch `to_image` is what the goldens pin. **Part B —
      the incremental commit**: a block seam (pwrite per page) appends only the **dirty** nodes a
      mutation introduced (clean subtrees keep their page id and are never rewritten) + the
      always-rewritten catalog, then publishes the new root by writing the **alternate meta slot**
      (`txid & 1`) — two fsyncs (body-before-meta) make a crash leave either the new snapshot or the
      immediately-prior one. Pages an old root drops **leak** (`page_count` only grows; reclamation
      is P6.2). Verified per-core (not goldens — the bytes depend on commit history): incremental
      growth bounded by tree height, slot alternation, torn-meta fallback to the prior durable
      snapshot, delete-heavy reopen. _(size: XL; deps: P5.1)_
- [x] **P6.2 — free-list / page reclamation** ✅ _(reconstruct-on-open form)_ — reuse the pages
      a new root no longer references (not version GC; still not MVCC). Landed across all three
      cores + the Ruby reference (the byte **format is unchanged** — reserved meta offset 28 stays
      `0`, so all 15 goldens and `format_version` 2 are untouched; reclamation only changes which
      indices an *incremental* commit allocates, never the from-scratch image the goldens pin).
      **On open** the free-list is reconstructed as `[2, page_count)` minus the pages reachable
      from the committed root (catalog chain + every table B-tree node — both already walked while
      loading). **During a commit** the allocator draws dirty/catalog pages from the free-list
      (**lowest index first**, so the bytes stay cross-core identical) before extending the file;
      a page leaves the list **only** by being allocated, which makes it live in the new committed
      version — so a free-list page is never reachable from the committed *or* the immediately
      prior (fallback) snapshot, and reuse stays **torn-write-safe**. **Gated on the
      oldest-live-snapshot watermark** (transactions.md §8): every free-list page was dead at the
      opened committed version and a single file-backed handle has
      `oldest_live_txid == committed.txid`, so `oldest_live_txid > T` holds **trivially**. Verified
      per-core (the bytes depend on commit history, like P6.1): a churn-then-reopen-then-churn
      reuses dead pages so `page_count` does not grow, reuse round-trips correctly, and a torn
      latest commit after reuse still falls back to the intact prior snapshot. **Deferred
      follow-ons** (where the watermark does real work): continuous *within-session* reclamation
      (return a commit's orphans to the free-list immediately, paired with file-backed reader
      sharing — needs O(dirty) orphan tracking to keep the commit incremental, or an O(live)
      reachable-set recompute) and **on-disk free-list persistence** (claim meta offset 28 so open
      skips the reachable-set walk — *persist later only for open speed*). _(size: L; deps: P6.1)_
- [x] **P6.3 — `page_read` cost unit + corpus cost re-baseline** ✅ — the store is now a
      page-backed B-tree (P6.1), so a distinct **`page_read`** unit was **added** to
      [spec/cost/schedule.toml](spec/cost/schedule.toml) alongside `storage_row_read` (not a
      rename — both fire on a scan; storage.md §6). Landed across all three cores. **Accrual rule**
      ([cost.md §3](spec/design/cost.md) "page_read"): the executor has no index/point-lookup path,
      so every `SELECT`/`DELETE`/`UPDATE` scan walks the table's **whole** B-tree → it charges
      `page_read` once per **node** (interior + leaf) in that tree — the structural **node count** —
      as a block **before** the table's `storage_row_read`s. An empty table (no root) charges none.
      The count is **deterministic + byte-identical across cores** because the in-memory B-tree **is**
      the on-disk one, node-for-page, whose node boundaries are a §8 byte contract (P6.1). It composes
      exactly like `storage_row_read`: a JOIN charges each materialized base table's node count once
      (self-join twice); a set op `lhs + rhs`; an uncorrelated subquery once (folded); a **correlated**
      subquery's inner re-scan **per outer row**. Counted as a **logical** page access (the node count),
      not a physical fetch, so a future buffer pool stays invisible to the deterministic cost (§13).
      **Re-baseline** landed **atomically across all three cores** (a §13 cross-core determinism
      contract): a new `--rebaseline` mode in the **Rust** conformance harness rewrote every `# cost:`
      directive (40 of 41 corpus files; 1 unchanged — an INSERT applying defaults scans nothing), and
      the unchanged Go/TS harnesses **independently re-verified 72/0/0** (the cross-core oracle — all
      cores agree on the new costs by construction). Per-core cost-assertion tests (select/insert/
      setops/subquery in each core) re-baselined to the same values. Byte format **untouched** (15
      goldens still byte-exact; `rake verify` clean). _(size: M; deps: P6.1; §13)_
- [x] **P6.4 — Buffer pool / demand paging** — ✅ **landed (all 3 cores, merged + pushed,
      `origin/master == febcc7c`).** The resident set is now a **bounded cache of leaf pages**
      with eviction (not the whole file), so a file far larger than RAM is served by paging in
      on demand. **Design** ([spec/design/pager.md](spec/design/pager.md)). **Decisions as built:**
      a **universal** buffer pool (every committed-tree leaf read paged, no full-residency fast
      path — results+cost the only contract, so the pool/eviction is NOT a §8 byte contract, like
      P5.3's per-core concurrency); reached **seam-foundation-first**. **Refinement that landed:**
      only **leaves** page — the **interior skeleton stays resident** — so `node_count` is
      computable from the resident skeleton (an OnDisk leaf counts as 1) **without faulting
      leaves**, which kept `page_read` accrual **structural in the executor** and cost
      **byte-identical to P6.3** (the predicted per-node-visit move was avoided; 72/0/0 unchanged).
      **Slices (all complete):**
  - [x] **P6.4a — the pager seam (no residency change).** Introduced the `Pager` (file + in-memory
        backings, file kept open for the handle's life; `read_block`/`write_block`/`sync`) and
        routed the whole-image load **and** the incremental commit (P6.1) through it; a buffer-pool
        scaffold existed but the loader still materialized the full tree, so results/cost/the 15
        goldens stayed **byte-unchanged**. De-risked the seam + keep-file-open lifecycle (`close`
        now closes the file).
  - [x] **P6.4b — lazy leaves + the bounded pool (the residency win).** `pmap` children became a
        lazy `Child` enum (`Resident | OnDisk(page_id)`; only leaves page); a clean leaf loads on
        demand through the bounded pool with **CLOCK** (second-chance) eviction, **no pins** (an
        in-flight reference + GC keeps an evicted-but-in-use node alive; clean nodes are immutable
        so re-load is harmless). The skeleton-resident loader (`open_paged`/`read_skeleton`) builds
        only interior pages resident. Rust needed an owned-rows step (a borrow can't outlive the
        pool lock) + `Result` plumbing; Go/TS shed that via GC. The resident set is now bounded.
  - [x] **P6.4c — budget config + hardening.** Handle-level **memory-budget API**:
        `open(path, { cache_pages / CachePages / cachePages })` (budget in leaf pages, default 1024,
        clamped ≥ 1) — a **handle setting**, not stored in the file (`spec/design/api.md` §2.1); a
        read-only `resident_leaves` gauge (≤ budget by construction); large-file tests (a DB far
        exceeding the budget opens / scans / mutates / round-trips with resident leaf count staying
        bounded under a repeated point-query workload).

      _(size: XL; deps: B-tree pages + incremental commit [P6.1] ✓; §9/§13)_
- [ ] **Streaming + spill-to-disk operators** — bound blocking operators (`ORDER BY`, hash
      `JOIN`, `GROUP BY`/aggregate, `DISTINCT`) by a memory budget and **spill to disk** when
      exceeded (external merge sort, grace hash join), so a query over larger-than-RAM data
      never materializes its whole input/output in memory. Pull-based row iteration is the
      enabler. _(size: XL; deps: paged storage; §9/§13)_
- [ ] **Large values — overflow pages + compression (TOAST-equivalent).** Designed in
      [spec/design/large-values.md](spec/design/large-values.md). Lift the `RECORD_MAX` / `u16`
      oversized-item ceilings (`0A000`) by pushing large `text`/`bytea`/`json`/`decimal` values
      **out-of-line onto an overflow-page chain**, optionally **compressed** first (a
      deterministic hand-rolled **LZ4-block** codec). **Build order: overflow first (Slice A),
      compression second (Slice B)** — both behind one `format_version` 3 design (reserve the
      compressed form codes in A so B is additive).
      - [x] **Slice A — overflow / out-of-line storage** ✅ **landed** (all 3 cores + Ruby ref):
            `format_version` 3 (extended presence tag `0x02` external-plain, 9-byte pointer,
            `page_type 4` chains, spill-only-when-forced planner — large-values.md §12),
            byte-exact goldens incl. `overflow_table.jed`, reconstruct-on-open reclamation of
            live chains, and the `page_read` cost accrual (chain pages folded into the scan's
            up-front block — cost.md §3). Lazy (read-on-touch) materialization and the
            per-touched-value cost refinement are tracked follow-ons (§7/§8.1).
      - [ ] **Slice B — transparent compression**: the deterministic LZ4-block codec + input→bytes
            fixtures, forms `0x03`/`0x04` (additive within v3), the compress decision +
            store-smaller rule, and the `value_decompress` cost unit. The compressor is
            hand-rolled per core (a library fails §8 cross-core byte-identity — large-values.md
            §6), so it needs **no** third-party dependency; any later proposal is gated on
            CLAUDE.md §14.

      Unblocks the `decimal` 1000-digit cap and the `json`/`array` headline types. _(size: L→XL;
      deps: B-tree pages [P6.1] ✓; §9/§13/§14)_
- [ ] **Crash-recovery hardening** — torn-meta fixtures exist; expand durability/recovery
      tests. WAL is deferred (COW + root-swap gives atomicity without one). _(size: M; §9)_

---

## Phase 7 — Embedding / host API surface

> The north star is an **embeddable library** (§1). The formal API + bind parameters have
> **landed** (`spec/design/api.md`); the browser/OPFS host remains. Parallelizable with most
> feature work.

- [x] **Formal public API** — ✅ **landed** (`spec/design/api.md`): `create`/`open` a database
      file, crash-safe explicit `commit` (temp + fsync + atomic rename + dir fsync) / `close`,
      `prepare` a statement, execute, iterate result rows via a `Rows` cursor, structured-error
      surface (+ class-58 host codes). Same shape across all three cores; back-compat
      `execute(db, sql)` kept. _(was: size L; §1)_
- [x] **Parameterized queries (`$1`)** end-to-end — ✅ **landed**: `$N` is lexed/parsed,
      context-typed at resolve (42P18 if indeterminate), bound two-phase before any scan, run
      through `prepare`/`execute`/`execute_params`. Per-impl surface — corpus stays literal-only
      (conformance.md §1.2); tested in-impl (`params` test per core). _(was: size M)_
- [ ] **Storage hosts** — Node `fs` host **built** (Phase 7, `impl/ts/src/file.ts`; Rust/Go use
      `std::fs`/`os` directly); build the **browser/OPFS** host (`FileSystemSyncAccessHandle`)
      and confirm native file-host parity (§9, storage.md §2). _(size: L; §9)_
- [x] **Cost ceiling (`max_cost`) + deterministic abort** — ✅ landed (all 3 cores). A handle
      `max_cost` setting (`set_max_cost`/`SetMaxCost`/`setMaxCost`, `0` = unlimited) aborts a
      statement with **`54P01`** (`cost_limit_exceeded`, class 54 program_limit_exceeded) the
      instant accrued cost reaches it. Enforced by `Meter::guard()` at the unbounded-work points
      (per scanned row, per produced row, per expression node, per aggregate fold) — `charge`
      stays a pure accrual chokepoint so the `# cost:` contract is byte-unchanged; the abort is an
      ordinary error (rolls back). Deterministic + cross-core identical; the `# max_cost:` corpus
      directive + `resource.cost_limit` capability + `resource/cost_limit.test` pin it (cost.md
      §6, api.md §8). _(§13)_
- [ ] **(Open question, not scheduled)** low-level direct access API beneath SQL
      (`getValue("table", key)`) — keep the seam open, don't build yet (§9). _(size: —)_

---

## Phase 8 — Testing & tooling infrastructure (§7)

> Cross-cutting; raises the honesty/coverage ceiling. Some pairs with earlier phases.

- [ ] **Differential-testing harness** vs PostgreSQL/SQLite oracles to bootstrap corpus
      cheaply (§7). **PARTIAL** — the **live-`db` oracle-import** tool is built
      (`scripts/oracle_import.rb`; `rake corpus:import/check`; override ledger
      `spec/conformance/oracle_overrides.toml`; conformance.md §5) and needs no §12 provisioning.
      *Remaining:* the **bulk** bootstrap from the *source* checkouts (gated on **user-initiated**
      reference provisioning §12 — never auto-provision) and a SQLite oracle. _(size: M remaining; §7)_
- [ ] **SQLancer-style metamorphic / generative testing** — finds logic bugs by synthesizing
      queries with known-correct answers. **PARTIAL** — the **NoREC** slice is built
      (`scripts/norec_gen.rb`; `rake corpus:norec_sweep`, in `rake ci`; conformance.md §8): a
      pushdown predicate vs a non-optimizable rewrite must agree, run on all three cores. *Remaining:*
      **TLP** (ternary-logic partitioning — suits the 3-valued NULL + aggregate surface), **PQS**
      (pivoted query synthesis — needs an in-harness expression evaluator), an automatic **reducer**,
      and **broader NoREC relations** (see the growth obligation below). _(size: L remaining; §7)_
- [ ] **Result-type assertion directive** — assert a column's precise declared type
      (`int16` vs `int32`) beyond the `I`/`T`/`R` render tag (deferred, conformance.md §7).
      _(size: S; §7)_
- [ ] **Corpus growth** — keep adding `.test` coverage as each feature lands (ongoing). Two
      **standing obligations** when a feature lands (conformance.md §5/§8): (a) on the
      PG-comparable surface, run `rake corpus:check` on the new `.test` and register any
      intentional divergence in the override ledger; (b) **when you add a query optimization or a
      new evaluable query shape, add a NoREC relation for it** to `norec_gen.rb` — the sweep does
      **not** discover new optimizations, and adding *seeds* does not add coverage. NoREC covers
      point-lookup + range pushdown, `LIMIT` short-circuit, JOIN base-table pk pushdown, and
      correlated-subquery pushdown today; future index/DISTINCT/aggregate pushdown (and any later
      optimization) are **not yet** covered.

---

## Phase 9 — Language reach: more supported languages (§2)

> **Goal here is best experience per language, not spec-hardening** — the differential core
> set (Rust + Go + TS) already does the honesty work (CLAUDE.md §2, spec/design/cores.md).
> Each language is **native or wrapped** per the best-experience rule (performance vs. clean
> integration). **Two pivots** decide it (spec/design/cores.md §2.1–§2.2): (1) host-function
> hotness — hot-path per-row favors native, coarse favors wrap; (2) parallelism — the §3
> immutable-snapshot read path is near-lock-free, so wrapping Rust hands every host
> Rayon-grade intra-query parallelism free (and dodges Swift's ARC-contention), while native
> is strong for C#/Java (GC-cheap sharing) and weak for Swift. Wrapping the safe Rust core is
> a **first-class** choice here, not an exception. Any native core still passes the full
> conformance contract (§7/§8); a wrap inherits it from Rust.

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
- [ ] **Design the host-function API vectorized/batched** up front — the single decision
      that keeps wrapping viable for any of the above (amortizes the per-row FFI upcall).
      _(size: M; §2, cross-cutting)_

---

## Ordering rationale & open tensions (for iteration)

- **Why Phase 1 first:** two canonical spec dirs (`grammar/`, `functions/`) are still
  empty, and a general expression evaluator is the prerequisite for almost everything in
  Phases 2 & 4. Cheap to do, unblocks the most.
- **Why the type system (Phase 3) is its own phase, not earlier:** it's *the product*, but
  most type work depends on the expression/operator substrate from Phase 1, and `decimal`
  (XL) shouldn't gate the SQL-shape features in Phase 2.
- **Tensions to decide:**
  - `NOT NULL` / `DEFAULT` are fundamental and easy — **done** (landed with the `INSERT` column
    list + `DEFAULT` keyword; constraints.md). (Was: pull them into Phase 2?)
  - `JOIN`s are arguably core SQL — **done** for `INNER`/`CROSS` (Phase 4); outer joins +
    aggregates remain. (Was: promote `JOIN`s ahead of aggregates?)
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
- **Encryption at rest (file-level).** Whole-file or per-page **encryption** is a door to
  keep open, not a scheduled feature (CLAUDE.md §9, storage.md §6). The block seam is the
  natural insertion point; crypto would come from a **vetted library, never hand-rolled**
  (§14). The only present requirement is that the on-disk format and storage seam not
  foreclose it (don't assume page bytes are plaintext-comparable on disk).
