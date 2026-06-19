# Conformance suite — design

> The reasoning behind the conformance corpus and its harness contract. The **corpus
> is the contract** between implementations (CLAUDE.md §7, §10): a feature is "make
> these entries pass." This doc defines (1) the file format, (2) the structured-error
> matching extension, (3) the three-axis taxonomy (suites / capabilities / profiles),
> (4) the determinism rules, and (5) the corpus-bootstrapping policy. The capability and
> profile *data* lives in [../conformance/manifest.toml](../conformance/manifest.toml),
> validated by `rake verify`; this doc is the *why*.

The corpus is the spine of the project (CLAUDE.md §7). Because there is no reference
implementation (CLAUDE.md §2), the only thing that says two cores agree is that they
produce identical results on the same shared, declarative tests. Everything here is in
service of that: one format, deterministic expected output, machine-legible failures.

## 1. Format: sqllogictest-style

Plain-text, declarative, one **record** per directive, records separated by blank lines.
Invented by SQLite to run identical tests across independent engines — exactly our
problem (CLAUDE.md §7). We use the standard record types:

- `statement ok` + SQL — the statement must execute without error.
- `statement error <sqlstate>` + SQL — the statement must fail; see §2.
- `query <coltypes> <sortmode> [label]` + SQL + `----` + expected values — the query
  must succeed and produce exactly the listed values.

Conventions, fixed here so every implementation renders identically:

- **coltypes** — one letter per result column: `I` integer, `B` boolean, `T` text, `D`
  decimal, `R` real. The corpus uses `I`, `B`, `T`, `D`, and now **`R`** (`f64` — the
  first binary float, CLAUDE.md §4). Unlike the others, an `R` column is compared **by value at
  a tolerance, not by string**: both sides are parsed to f64 and considered equal iff bit-equal
  or within a small ULP/relative tolerance, with `NaN == NaN`, `±Inf` exact, and `-0 == +0`
  ([float.md](float.md) §9, [determinism.md](determinism.md) §6). This single rule absorbs the
  cross-core rendering-layout difference and the exempted transcendental last-ULP divergence
  (`f64` is the first type exempted from cross-core byte-identity for *computed/rendered
  values*; its *storage*, *ordering*, *kernel*, *exact-sum aggregates*, and *cost* stay exact).
  The letter is a **rendering** tag (how a value
  is printed), *not* a type assertion — asserting the precise resolved type (`i16` vs
  `i32`) is the separate **`# types:`** directive (below; the decimal `numeric(p,s)` typmod
  granularity stays deferred, §7). Types that render as a printable-ASCII string reuse the `T`
  tag accordingly: `bytea` (the `\x…` lowercase-hex form) and `uuid` (the canonical
  `8-4-4-4-12` lowercase form) are `T`-tag values.
- **values** — printed one per line, **row-major** (row 1's columns, then row 2's, …). A
  single integer renders as its shortest decimal form (no leading zeros, leading `-` for
  negatives). A **boolean renders as the literal `true` or `false`** (lowercase; never
  `t`/`f`, `0`/`1`, or host casing — a CLAUDE.md §8 determinism decision). **NULL renders as
  the literal `NULL`** (for every column type, boolean included — a NULL boolean is unknown,
  printed `NULL`, not `false`).
- **empty result** — the `----` separator followed by no value lines (the record ends at
  the next blank line).
- **sortmode** — `nosort` (compare in returned order), `rowsort` (compare as multisets — sort
  both sides first), or `valuesort`. **Row order is part of the contract only under `ORDER
  BY`** (CLAUDE.md §8/§10), so: a query **with** an order-determining `ORDER BY` uses `nosort`
  (the SQL fixes the order); a multi-row query **without** one uses **`rowsort`** (exact rows,
  order not asserted). Never pin row order with `nosort` on a query that lacks `ORDER BY` —
  that would test storage/iteration order the engine does not promise.
- **hashing** — large result sets may be replaced by `<N> values hashing to <md5>` with a
  `hash-threshold <N>` control record. Unused in tiers 1–3 (result sets are tiny); listed
  here so the format is complete.
- **conditionals** — `skipif <capability>` / `onlyif <capability>` immediately before a
  record gate it on a capability (§3).
- **`# requires:` header** — each file declares the capabilities it needs on one comment
  line: `# requires: ddl.create_table, dml.insert, types.i16`. This is the file-level
  gate (§3). It is a **comment**, so the stock runner ignores it (§1.1); our harness reads
  it. Exactly one per file; the checker enforces this.
- **`# names:` directive** — an optional `# names: id, total, ?column?` comment that binds
  to the **next `query` record** and asserts that query's output column names, in order, as
  rendered strings. It mirrors the `# cost: N` directive (CLAUDE.md §13): both are comments
  the stock runner ignores (§1.1), each is consumed independently by the next record, and a
  `query` may carry either, both, or neither. Output names are a determinism surface — the
  rule that fixes them (bare column → canonical name, `expr AS alias` → alias, `*` → column
  names, any other expression → `?column?`) lives in [grammar.md](grammar.md) §8. The
  directive must precede a `query`, never a `statement` (a statement has no result columns).
- **`# types:` directive** — an optional `# types: i16, i32, decimal` comment that binds
  to the **next `query` record** and asserts each output column's **precise resolved type**, in
  order, as its canonical name. This is deliberately the assertion the **coltypes** *rendering*
  tag is **not**: the tag says how a value *prints*, so the three integer widths `i16`/`i32`/
  `i64` all carry the `I` tag and are indistinguishable by value alone — `# types:` pins the
  width, so a cross-core divergence in the integer **promotion tower**, a `CAST` target, or a
  comparison's result type fails here even when the printed rows agree (the §8 promotion-matrix
  hotspot, made assertable). Like `# names:`/`# cost:` it is a comment the stock runner ignores
  (§1.1), consumed independently by the next record, and must precede a `query`, never a
  `statement`. The names are the canonical scalar-type ids (`i16`/`i32`/`i64`/`text`/
  `boolean`/`decimal`/`bytea`/`uuid`/`timestamp`/`timestamptz`, `unknown` for an untyped NULL
  column), from the type system ([types.md](types.md) §1, [compare.toml](../types/compare.toml)).
  The asserted type is the resolved **scalar** type — for `decimal` the unconstrained `decimal`,
  **not** the `numeric(p,s)` typmod (the resolved expression type does not carry the display
  typmod; §7 records that finer granularity as deferred). Coverage: `suites/types/result_types.test`.

### 1.1 Why stay format-compatible

CLAUDE.md §12 keeps `sqllogictest-rs` checked out as a reference runner for `impl/rust`'s
harness. We therefore keep `.test` files parseable by the stock runner: our **extensions
are additive** — either a comment the stock runner skips (the `# requires:` header), data
outside the `.test` files (the manifest), or syntax chosen to *also* parse as standard
sqllogictest (the error code, §2). Do not invent `.test` syntax that the stock runner
would reject.

### 1.2 Parameters

The corpus uses **literal SQL** (`WHERE id = 1`), never bound parameters. The
parameterized lookup API implied by CLAUDE.md §11 step 5 (`SELECT ... WHERE pk = $1`) has
**landed** as each implementation's own host-API surface ([api.md](api.md), grammar.md §5):
`$N` placeholders are parsed and bound through `prepare`/`execute`, with a parameter's type
inferred from context (`42P18` if indeterminate). That surface — and its `42P18` code — is
tested **in-impl**, not by the shared corpus, which still fixes **semantics** with literal
SQL only, not the binding API.

## 2. Structured-error matching

Errors are structured data, not free text (CLAUDE.md §5, §10), so matching is on the
**code**, never the prose:

- `statement error <sqlstate>` — the statement must raise an error whose SQLSTATE equals
  `<sqlstate>` (a code from [../errors/registry.toml](../errors/registry.toml), e.g.
  `22003` for integer overflow).
- The message *text* is informational and may change; it is never matched.
- **Compatibility:** every implementation renders an error with its SQLSTATE present in
  the message string, so `statement error 22003` also matches as a plain regex under the
  stock `sqllogictest-rs` runner. The structured match (our harness) and the regex match
  (stock runner) agree by construction.

## 3. The three-axis taxonomy: suites, capabilities, profiles

The honesty mechanism must tolerate implementations advancing at different speeds — Go
may run ahead while TS catches up — without the whole suite reading as broken
(CLAUDE.md §7). The earlier "numbered tiers" conflated three different things — how tests
are *organized*, what an impl *can do*, and what milestone it *targets* — so they are now
separated into three independent axes:

1. **Suites** — the [`suites/`](../conformance/suites/) directory tree. Purely
   *organizational*: tests are grouped by feature area (`query/`, `null/`, `types/`,
   `cast/`, `compare/`). A suite says nothing about gating; it just answers "what area is
   this test about." New areas are new subdirectories.

2. **Capabilities** — fine-grained features named with a dotted id (`types.i64`,
   `cast.explicit`, `null.three_valued`). This is the **gating** axis:
   - Each implementation **declares the set of capabilities it supports.**
   - Each `.test` file **declares the capabilities it requires** via its `# requires:`
     header (§1). Per-record `skipif`/`onlyif <capability>` gate individual records.
   - A test runs for an implementation **iff** the impl supports every capability the
     test requires. That is the whole gate — no test runs against an engine that hasn't
     declared it can handle it, so an incomplete engine reads as "fewer tests run," never
     as "suite broken."

3. **Profiles** — named, cumulative **bundles of capabilities** = conformance *levels* an
   implementation targets. `includes` inherits another profile's capabilities, so they
   stack. An implementation **meets** a profile iff its declared capability set is a
   superset of the profile's (transitive) capabilities. Profiles are how we ask "is this
   engine at `core` yet?" without enumerating capabilities by hand.

Capabilities and profiles are **data**, in
[../conformance/manifest.toml](../conformance/manifest.toml) (data over code, CLAUDE.md §5);
suites are the filesystem. All three are **test-time only** — the harness reads them; no
shipped engine does. `rake verify` runs [../conformance/verify.rb](../conformance/verify.rb),
which checks the taxonomy is internally coherent: every required/profiled capability is
defined, profile `includes` form no cycles, every `.test` has exactly one `# requires:`
line, and no capability is defined but unused.

The foundational profiles (the manifest now defines **18** in total — see below):

| Profile | Adds | Maps to |
|---|---|---|
| `core` | CREATE TABLE (+PK) / INSERT / SELECT / `WHERE pk =` / `ORDER BY` / `IS [NOT] NULL` / 3-valued NULL / insert overflow trap, integers only. | The CLAUDE.md §11 step-5 "it's alive" milestone. |
| `casts` | `core` + explicit `CAST` narrowing (fits, and traps `22003` when it doesn't). | [../types/casts.toml](../types/casts.toml). |
| `comparison` | `core` + cross-type integer comparison via the promotion tower (`<`, `>`, `=`). | [../types/compare.toml](../types/compare.toml). |
| `expression` | `comparison` + the general expression substrate: integer arithmetic (`+ - * / %`, unary `-`, precedence, parens; traps `22003`/`22012`), the expression-only `boolean` type (`TRUE`/`FALSE`, comparisons-as-values), `AND`/`OR`/`NOT` Kleene connectives, and `IS [NOT] DISTINCT FROM` (NULL-safe equality). | [../functions/catalog.toml](../functions/catalog.toml), [../design/types.md](../design/types.md) §9. |

These build up cumulatively to the later profiles — `mutation`, `decimal`, `functions`,
`joins`/`outer_joins`, `constraints`, `aggregates`/`grouping`/`having`, `predicates`,
`timestamps`, `set_operations`, `transactions`, and `subqueries` (18 total). The
[manifest.toml](../conformance/manifest.toml) is the authoritative, complete list.

## 4. Determinism rules

The agent loop and cross-impl sync both depend on bit-reproducibility (CLAUDE.md §10).
Every corpus entry MUST obey:

- **Ordered output.** A multi-row query either carries an order-determining `ORDER BY` (and
  uses `nosort`) **or** uses `rowsort` (exact multiset, order unasserted). No `nosort` entry
  may depend on storage or iteration order — row order is contractual only under `ORDER BY`
  (CLAUDE.md §8).
- **One canonical name, one code.** Types print under their canonical id; each error
  condition has exactly one SQLSTATE.
- **No *unledgered* nondeterminism.** Determinism is **default-deny** ([determinism.md](determinism.md)
  §1): no wall-clock, no random, no hashmap-order leakage — *unless* the behavior is an entry in
  the determinism-exception ledger ([../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml))
  with a stated blast radius and test mechanism. Two relaxations are exercised today: `f64`
  (class **A**, below) and the UUID generators (class **B**); everything else stays fully
  deterministic and cross-core byte-identical.
- **Floats** are the class-**A** ledgered exception. `f64` ([float.md](float.md)) is exempt from
  cross-core byte-identity for *computed/rendered values only* — compared via the `R` tag's
  tolerant rule (§1). Its *storage* bytes, *total order*, *arithmetic kernel*, *exact-sum
  `SUM`/`AVG`*, and *cost/names/types* remain exact and cross-core (so a float query still carries
  `# cost:`). `decimal` stays the exact path (`1.50` prints `1.50`, a `D`-tag value); a value is
  an `R`-tag value only when it is genuinely `f64`.
- **The UUID generators** are the class-**B** ledgered exception, but they stay **exact** in the
  corpus: `uuidv4()` / `uuidv7()` run on a host-injected **random + clock seam** (two functions —
  [entropy.md](entropy.md)), so a record pins both with the **`# seed: N`** and **`# clock: N`**
  directives (comments the stock runner ignores, §1.1, bound to the next record and reset after —
  like `# cost:`): the harness injects the engine's provided deterministic source seeded with `N`
  (and a fixed clock) and asserts the output byte-for-byte across all cores (the spec'd splitmix64
  source makes the injected path identical). A generator record WITHOUT a `# seed:` (and, for
  `uuidv7`, a `# clock:`) is non-conformant. `# cost:` is exact and source-independent. Production's
  default draws from the OS CSPRNG per value + the wall clock; only those raw reads are
  non-deterministic (the ledger entries).
- **The current-time functions** (`now()` / `current_timestamp` / `clock_timestamp()`,
  [entropy.md](entropy.md) §5) are the other class-**B** ledgered exception, also kept **exact** in
  the corpus by injecting the clock seam. A record pins the clock with **`# clock: N`** (a fixed
  instant) — enough for `now()` / `current_timestamp` (STABLE, one read) and for `clock_timestamp()`
  under a frozen clock — or with **`# clock_advance: start,step`** (an advancing clock that returns
  `start, start+step, …`, one increment per read in expression-evaluation order). The advancing form
  is what makes the VOLATILE `clock_timestamp()`'s per-call reads deterministic *and* distinguishable
  from the statement-stable `now()` (under it, two `now()` are equal while two `clock_timestamp()`
  differ). Both directives are stock-runner-ignored comments, bound to the next record and reset
  after — like `# clock:`/`# cost:`. NOT oracle-imported (PG's wall clock differs); `# cost:` is exact
  and clock-independent. Production reads the wall clock.
- **Canonical boolean spelling.** A boolean prints as exactly `true`/`false` (NULL as
  `NULL`); no core may emit `t`/`f`, `0`/`1`, or host-cased variants.

## 5. Bootstrapping policy

- The corpus is **predominantly hand-authored.** Integer semantics are small and fully
  known, so the expected output is written directly and reviewed as the contract.
- **Oracle-import** against the live PostgreSQL service is **available** (`scripts/oracle_import.rb`;
  `rake corpus:import[file]` fills a `.test`'s expected rows/error codes from PG, `rake
  corpus:check[file]` re-derives and diffs without writing). It talks **only to the running
  `db` service**, never the source checkout, so it does **not** trip the §12
  reference-provisioning gate, and it is **psql-only** (no `pg` gem — no §14 dependency). It is
  an authoring aid + a standing drift check, **not** a query generator (that is the metamorphic
  generator, §8). It cannot derive `# cost:` (PG has no notion of jed's cost units), so cost
  assertions stay hand-authored. Each record replays the file's prior **state-changing**
  records as its prefix: every `statement ok`, plus every **row-returning DML** `query`
  record (`INSERT`/`UPDATE`/`DELETE ... RETURNING` — grammar.md §32; a `SELECT` query
  record is side-effect-free and is not replayed). So a record that *fails* on PG while jed
  runs it (a documented divergence) must sit **LAST** in its file, or it poisons the replay
  prefix of everything after it.
- **Intentional divergences are a machine-checked ledger.** PostgreSQL is the *default*, not a
  compatibility target (CLAUDE.md §1): where jed deliberately differs — the strict type system,
  a documented narrowing — the divergence is recorded in
  [oracle_overrides.toml](../conformance/oracle_overrides.toml) with a reason. `corpus:check`
  stays silent on a divergence that has an entry and **warns ("add an override") on one that
  does not**, so the ledger cannot fall out of date. When you add a `.test` on the
  PG-comparable surface, run `corpus:check` on it and register any divergence in the sidecar.
- **Coltype + type coverage.** The importer derives each result column's render tag from PG's
  `\gdesc` and covers every shipped scalar — integers (`I`), `boolean` (`B`), `numeric` (`D`),
  and `text` / `bytea` / `uuid` / **`timestamp` / `timestamptz`** (`T`). The session zone is pinned
  to **UTC** for each replay so `timestamptz` renders `+00`, matching jed's instant model
  (timestamp.md); zoneless `timestamp` is zone-independent. A query PG cannot even *describe* — a
  **jed extension PostgreSQL has no overload for**, e.g. `MIN`/`MAX` over `uuid` or `boolean` (jed
  defines those orderings; PG ships no such aggregate) — is left at its committed rows with a
  warning, and is recorded in the ledger like any other divergence rather than crashing the import.
- **Never auto-provision references or run heavy oracles** (CLAUDE.md §12). The *source*
  checkouts and any bulk import remain explicit, user-initiated steps; the live-`db` oracle
  above is the always-available path that needs no provisioning.

## 6. Running the corpus

Each implementation ships a **thin harness** that: reads the manifest, determines the
capabilities the implementation declares, runs every `.test` file whose `# requires:`
capabilities are all satisfied (skipping the rest), executes each record, and compares
output under the rules above. Reporting a profile = checking that every test gated by that
profile's capabilities passes. All three cores (Rust, Go, TS) ship this harness today.

## 7. Open / deferred

- **Result-type assertions** — ✅ **resolved**: the `# types:` directive (§1) asserts each result
  column's precise resolved type (`i16` vs `i32`, the family) beyond the `I`/`T`/`D` rendering
  tag, exposed through each core's `Outcome::Query` column-types accessor. It pins the integer
  promotion tower / `CAST` target / comparison result type — the cross-core divergence the value
  tag alone cannot catch. Coverage in `suites/types/result_types.test`. **Still deferred:** the
  finer `decimal` typmod granularity (`numeric(p,s)` vs bare `decimal`) — the resolved expression
  type carries the value's display *scale* but not a column *typmod*, so a decimal result asserts
  as the unconstrained `decimal`; a directive that distinguishes `numeric(10,2)` would need the
  typmod threaded through expression type resolution.
- **Integer-literal typing** — ✅ **resolved**: a bare integer literal is an *untyped
  constant* that adapts to its context and traps `22003` when its value does not fit (so
  `WHERE small = 100000`, with `small i16`, is a type error, not a silent non-match). See
  [../design/types.md](../design/types.md) §6; coverage in `suites/types/literals.test`.
- **Boolean results / connectives** — ✅ **resolved**: the `boolean` type (now storable —
  types.md §9), comparisons-as-values, and `AND`/`OR`/`NOT` Kleene connectives landed with the
  general expression substrate. Rendered under the `B` tag (§1); the `expression` profile (§3)
  gates them; coverage in `suites/expr/`. Boolean as a *storable column type* remains
  deferred (types.md §10).
- **Render-tag breadth** — `I`, `B`, `T` (text), `D` (decimal), and now `R` (`f64` — binary
  float, compared at tolerance §1) are all in use (CLAUDE.md §8).

## 8. Metamorphic generator (SQLancer-style) — and the obligation to grow it

[scripts/norec_gen.rb](../../scripts/norec_gen.rb) generates self-checking metamorphic tests.
Expected rows are known **by construction** from the generated data, so no oracle (PG or otherwise)
is consulted. Each seed emits one file per scenario; `rake corpus:norec_sweep` runs a fixed,
reproducible sweep (seeds 1..N × scenarios) on **all three cores**, so each test is checked
**metamorphically** (the equivalent forms agree) *and* **differentially** (the cores agree). It is
in the `rake ci` gate. Two families of relation are generated:

- **NoREC** (Non-optimizing Reference Engine Construction): a query that triggers an optimization
  and a *semantically-equivalent* form that does **not** must return identical rows. The canonical
  case: jed's planner pushes a predicate to a B-tree seek/range only when the primary key appears as
  a **bare column** (`detect_pk_bound`), so `id = K` is pushed down while `id + 0 = K` (a `BinaryOp`)
  full-scans — two different code paths that must agree. One **scenario per optimization** (the
  covered list below).
- **TLP** (Ternary-Logic Partitioning): for **any** predicate `p`, three-valued logic places every
  row in exactly one of `WHERE p` (TRUE), `WHERE NOT p` (FALSE), or `WHERE (p) IS NULL` (UNKNOWN), so
  the three partitions `UNION ALL` must reconstruct the unpartitioned table (and `COUNT` over the
  whole equals the partition counts summed). Unlike NoREC this is **not** an optimized-vs-unoptimized
  pair — it is an independent oracle for the 3-valued NULL logic itself (comparison-with-NULL, the
  Kleene `AND`/`OR`/`NOT` connectives, `IS NULL` — the §8 divergence hotspot), well-suited to jed's
  NULL surface. The generator computes the partition by construction with the same Kleene rules jed
  must implement; a bug shows up as a partition that fails to reconstruct the whole. The `tlp`
  scenario covers comparison / equality / Kleene-`AND` / Kleene-`OR` / arithmetic-NULL predicates and
  the `COUNT(*)` / `COUNT(expr)` aggregate forms; `SUM`/`MIN`/`MAX`/`AVG` aggregate-TLP is **deferred**
  (combining per-partition results needs a `COALESCE`/`LEAST`/`GREATEST` jed does not have yet).

**Why this catches what the differential cores cannot.** Running every `.test` on Rust/Go/TS
catches the cores *disagreeing*; it is blind to a bug **all three share**. A metamorphic
relation is an independent oracle — it can fail even when all cores agree.

**The growth obligation (this is load-bearing — do not let it ossify).** The sweep covers only
the query shapes the generator **emits**. It does **not** discover new optimizations on its own,
and **adding seeds does not add feature coverage** — a seed only varies the data; coverage grows
only when a new metamorphic *relation* is added to the generator. So: **when you land a query
optimization or a new evaluable query shape, add a NoREC relation for it** (an optimized form +
a rewrite the planner cannot optimize), in the same change. A passing sweep that silently tests
only yesterday's optimizations is false confidence (CLAUDE.md §10 "no silent caps").

- **Covered today** (one scenario each): **pushdown** — point-lookup (`pk = K`) and range
  (`pk BETWEEN a AND b`) on an integer primary key; **limit** — `LIMIT` short-circuit, where the
  windows of an `ORDER BY`-on-pk query (`LIMIT a`, `OFFSET a`, boundaries) must reconstruct the
  ordered whole; **join** — JOIN base-table pk pushdown, a constant `pk = K` bounding one
  relation's scan (INNER, plus a preserved-side LEFT predicate whose NULL-extension must survive
  the pushdown), defeated by `pk + 0 = K`; **correlated** — a correlated subquery whose inner pk
  equals an outer column (`inr.id = o.k`) bounds the inner re-scan to a per-outer-row seek
  (through EXISTS / scalar / IN, including a NULL outer key), defeated by `inr.id + 0 = o.k`;
  **index** — a secondary-index equality (`v = K` on an indexed column) fetches via the index
  tree + per-row point lookups ([indexes.md §5](indexes.md)), defeated by `v + 0 = K`, checked
  across UPDATE/DELETE maintenance and a NULL indexed value (3VL through the index); **tlp** —
  ternary-logic partitioning (above), an independent oracle for 3-valued NULL logic rather than an
  optimization pair.
- **NOT yet covered (needs a new relation):** any future index *range* / multi-column-prefix
  bound, DISTINCT / aggregate pushdown, or other optimization added later; on the TLP side,
  `SUM`/`MIN`/`MAX`/`AVG` aggregate partitioning (blocked on `COALESCE`/`LEAST`/`GREATEST`) and a
  `GROUP BY`-level TLP. Each is a future relation the sweep does **not** yet exercise — add a
  scenario when it lands.

**Reducing a discovered failure.** Generation is seeded, so a failure reproduces deterministically
(CLAUDE.md §10), but a failing file is large and noisy. [scripts/reduce.rb](../../scripts/reduce.rb)
(`rake corpus:reduce[file,core]`) shrinks it automatically: **delta-debugging (ddmin) over the
file's records** — the blank-line-separated `statement`/`query` blocks; the `# requires:` header is
fixed — accepting a smaller record set iff the chosen core's harness still reports the
**byte-identical failure signature** (the `FAIL …` message + SQL + expected/actual). That strict
oracle is what makes record removal safe despite hard-coded expected rows: dropping the CREATE makes
the failing query *error* (different message → rejected), dropping the INSERT changes its `actual:`
(different signature → rejected), so prerequisites and any state-changing record the failure depends
on are retained automatically while unrelated records are stripped. The result is the minimal
{CREATE, INSERT, …, failing query}, ready to commit as a regression `.test` (the fuzzer discovers;
the reducer distills; the committed `.test` is the durable artifact). It reduces at record
granularity only — shrinking the data or the failing SQL would change the expected/actual and thus
the signature, so finer trimming is left to the author. A fixed synthetic case guards the reducer in
`rake ci` (`rake corpus:reduce_selftest`).

Further SQLancer oracles remain open (TODO.md Phase 8): **PQS** (pivoted query synthesis — needs an
in-harness expression evaluator) and broader NoREC/TLP relations as new optimizations land.
