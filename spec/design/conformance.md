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
  decimal, `R` real. The corpus uses `I`, `B`, `T`, and `D` (integers, boolean, text, and the
  exact decimal — all storable, CLAUDE.md §4); `R` (binary float) is reserved and may never be
  used until a float type exists (§4). The letter is a **rendering** tag (how a value
  is printed), *not* a type assertion — asserting the precise declared type (`int16` vs
  `int32`, or a decimal's `numeric(p,s)`) is a planned directive, deferred (§7). Types that
  render as a printable-ASCII string reuse the `T` tag accordingly: `bytea` (the `\x…`
  lowercase-hex form) and `uuid` (the canonical `8-4-4-4-12` lowercase form) are `T`-tag values.
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
  line: `# requires: ddl.create_table, dml.insert, types.int16`. This is the file-level
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

### 1.1 Why stay format-compatible

CLAUDE.md §12 keeps `sqllogictest-rs` checked out as a reference runner for `impl/rust`'s
harness. We therefore keep `.test` files parseable by the stock runner: our **extensions
are additive** — either a comment the stock runner skips (the `# requires:` header), data
outside the `.test` files (the manifest), or syntax chosen to *also* parse as standard
sqllogictest (the error code, §2). Do not invent `.test` syntax that the stock runner
would reject.

### 1.2 Parameters

The corpus uses **literal SQL** (`WHERE id = 1`), never bound parameters. The
parameterized lookup API implied by CLAUDE.md §11 step 5 (`SELECT ... WHERE pk = $1`) is
each implementation's own surface and is tested in-impl; the corpus fixes **semantics**,
not the binding API.

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

2. **Capabilities** — fine-grained features named with a dotted id (`types.int64`,
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

Current profiles:

| Profile | Adds | Maps to |
|---|---|---|
| `core` | CREATE TABLE (+PK) / INSERT / SELECT / `WHERE pk =` / `ORDER BY` / `IS [NOT] NULL` / 3-valued NULL / insert overflow trap, integers only. | The CLAUDE.md §11 step-5 "it's alive" milestone. |
| `casts` | `core` + explicit `CAST` narrowing (fits, and traps `22003` when it doesn't). | [../types/casts.toml](../types/casts.toml). |
| `comparison` | `core` + cross-type integer comparison via the promotion tower (`<`, `>`, `=`). | [../types/compare.toml](../types/compare.toml). |
| `expression` | `comparison` + the general expression substrate: integer arithmetic (`+ - * / %`, unary `-`, precedence, parens; traps `22003`/`22012`), the expression-only `boolean` type (`TRUE`/`FALSE`, comparisons-as-values), `AND`/`OR`/`NOT` Kleene connectives, and `IS [NOT] DISTINCT FROM` (NULL-safe equality). | [../functions/catalog.toml](../functions/catalog.toml), [../design/types.md](../design/types.md) §9. |

## 4. Determinism rules

The agent loop and cross-impl sync both depend on bit-reproducibility (CLAUDE.md §10).
Every corpus entry MUST obey:

- **Ordered output.** A multi-row query either carries an order-determining `ORDER BY` (and
  uses `nosort`) **or** uses `rowsort` (exact multiset, order unasserted). No `nosort` entry
  may depend on storage or iteration order — row order is contractual only under `ORDER BY`
  (CLAUDE.md §8).
- **One canonical name, one code.** Types print under their canonical id; each error
  condition has exactly one SQLSTATE.
- **No nondeterminism.** No wall-clock, no random, no hashmap-order leakage.
- **No floats.** The scalar set is integers, text, boolean, and the exact `decimal` — **no
  binary float** (`R`), so the float-formatting divergence (CLAUDE.md §8) cannot arise.
  `decimal` renders deterministically as a canonical base-10 string preserving its display
  scale (`1.50` prints `1.50`; [decimal.md](decimal.md) §6), so it is a `D`-tag value, never an
  `R`-tag one.
- **Canonical boolean spelling.** A boolean prints as exactly `true`/`false` (NULL as
  `NULL`); no core may emit `t`/`f`, `0`/`1`, or host-cased variants.

## 5. Bootstrapping policy

- The current corpus is **hand-authored.** Integer semantics are small and fully known, so
  the expected output is written directly and reviewed as the contract.
- **Differential bootstrapping** against PostgreSQL/SQLite oracles (CLAUDE.md §7) is
  deferred and optional. When used, where our semantics intentionally diverge from the
  oracle we override the expected output by hand and document why.
- **Never auto-provision references or run heavy oracles** (CLAUDE.md §12). Provisioning
  the reference checkouts or running a bulk oracle import is an explicit, user-initiated
  step.

## 6. Running the corpus

Each implementation ships a **thin harness** that: reads the manifest, determines the
capabilities the implementation declares, runs every `.test` file whose `# requires:`
capabilities are all satisfied (skipping the rest), executes each record, and compares
output under the rules above. Reporting a profile = checking that every test gated by that
profile's capabilities passes. Harnesses arrive with the first vertical slice
(CLAUDE.md §11 step 5).

## 7. Open / deferred

- **Result-type assertions** — a directive to assert a result column's precise declared
  type (`int16` vs `int32`), beyond the `I`/`T`/`R` rendering tag. Deferred.
- **Integer-literal typing** — ✅ **resolved**: a bare integer literal is an *untyped
  constant* that adapts to its context and traps `22003` when its value does not fit (so
  `WHERE small = 100000`, with `small int16`, is a type error, not a silent non-match). See
  [../design/types.md](../design/types.md) §6; coverage in `suites/types/literals.test`.
- **Boolean results / connectives** — ✅ **resolved**: the `boolean` type (now storable —
  types.md §9), comparisons-as-values, and `AND`/`OR`/`NOT` Kleene connectives landed with the
  general expression substrate. Rendered under the `B` tag (§1); the `expression` profile (§3)
  gates them; coverage in `suites/expr/`. Boolean as a *storable column type* remains
  deferred (types.md §10).
- **Render-tag breadth** — `I`, `B`, `T` (text), and `D` (decimal) are in use; `R` (binary
  float) stays reserved and unused until a float type exists, if ever (CLAUDE.md §8).
