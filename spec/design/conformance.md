# Conformance suite — design

> The reasoning behind the conformance corpus and its harness contract. The **corpus
> is the contract** between implementations (CLAUDE.md §7, §10): a feature is "make
> these entries pass." This doc defines (1) the file format, (2) the structured-error
> matching extension, (3) the tier + capability-flag system, (4) the determinism rules,
> and (5) the corpus-bootstrapping policy. The tier/flag *data* lives in
> [../conformance/manifest.toml](../conformance/manifest.toml); this doc is the *why*.

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

- **coltypes** — one letter per result column: `I` integer, `T` text, `R` real. Tier 1–3
  use only `I` (integers, CLAUDE.md §4). The letter is a **rendering** tag (how a value is
  printed), *not* a type assertion — asserting the precise declared type (`int16` vs
  `int32`) is a planned directive, deferred (§6).
- **values** — printed one per line, **row-major** (row 1's columns, then row 2's, …). A
  single integer renders as its shortest decimal form (no leading zeros, leading `-` for
  negatives). **NULL renders as the literal `NULL`.**
- **empty result** — the `----` separator followed by no value lines (the record ends at
  the next blank line).
- **sortmode** — `nosort` (compare in returned order), `rowsort`, or `valuesort`. We
  **prefer `nosort` with an explicit `ORDER BY`** so the SQL, not the harness, fixes the
  order (determinism — CLAUDE.md §8/§10). `rowsort` is acceptable when order is genuinely
  irrelevant.
- **hashing** — large result sets may be replaced by `<N> values hashing to <md5>` with a
  `hash-threshold <N>` control record. Unused in tiers 1–3 (result sets are tiny); listed
  here so the format is complete.
- **conditionals** — `skipif <flag>` / `onlyif <flag>` immediately before a record gate it
  on a capability flag (§3).

### 1.1 Why stay format-compatible

CLAUDE.md §12 keeps `sqllogictest-rs` checked out as a reference runner for `impl/rust`'s
harness. We therefore keep `.test` files parseable by the stock runner: our **extensions
are additive and live outside the `.test` files** (the tier/flag manifest), or are chosen
to *also* parse as standard sqllogictest (the error code, §2). Do not invent `.test`
syntax that the stock runner would reject.

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

## 3. Tiers and capability flags

The honesty mechanism must tolerate implementations advancing at different speeds — Go
may run ahead while TS catches up — without the whole suite reading as broken
(CLAUDE.md §7). Mechanism:

- A **capability flag** names one feature an implementation may support (e.g.
  `cast.explicit`, `types.int64`).
- Each implementation **declares the set of flags it supports**.
- Each **tier** declares the flags it `requires` (and optionally `extends` a lower tier,
  inheriting its requirements). An implementation runs a tier iff it supports every
  required flag, transitively.
- Within a file, `skipif`/`onlyif <flag>` gate an individual record.

The flags and tiers are **data**, in [../conformance/manifest.toml](../conformance/manifest.toml)
(data over code, CLAUDE.md §5). Like every spec table the manifest is **test-time only** —
the harness reads it; no shipped engine does.

Current tiers:

| Tier | Scope | Maps to |
|---|---|---|
| `tier1_core` | CREATE TABLE (+PK) / INSERT / SELECT / `WHERE pk =` / `ORDER BY` / `IS [NOT] NULL` / 3-valued NULL / insert overflow trap, integers only. | The CLAUDE.md §11 step-5 "it's alive" milestone. |
| `tier2_casts` | Explicit `CAST` narrowing (fits, and traps `22003` when it doesn't). Extends `tier1_core`. | [../types/casts.toml](../types/casts.toml). |
| `tier3_comparison` | Cross-type integer comparison via the promotion tower (`<`, `>`, `=`). Extends `tier1_core`. | [../types/compare.toml](../types/compare.toml). |

## 4. Determinism rules

The agent loop and cross-impl sync both depend on bit-reproducibility (CLAUDE.md §10).
Every corpus entry MUST obey:

- **Ordered output.** Any query returning more than one row carries an `ORDER BY` (or uses
  `rowsort`/`valuesort`). No entry may depend on storage or iteration order (CLAUDE.md §8).
- **One canonical name, one code.** Types print under their canonical id; each error
  condition has exactly one SQLSTATE.
- **No nondeterminism.** No wall-clock, no random, no hashmap-order leakage.
- **No floats.** Tiers 1–3 are integer-only, so the float-formatting divergence
  (CLAUDE.md §8) cannot arise.

## 5. Bootstrapping policy

- Tiers 1–3 are **hand-authored.** Integer semantics are small and fully known, so the
  expected output is written directly and reviewed as the contract.
- **Differential bootstrapping** against PostgreSQL/SQLite oracles (CLAUDE.md §7) is
  deferred and optional. When used, where our semantics intentionally diverge from the
  oracle we override the expected output by hand and document why.
- **Never auto-provision references or run heavy oracles** (CLAUDE.md §12). Provisioning
  the reference checkouts or running a bulk oracle import is an explicit, user-initiated
  step.

## 6. Running the corpus

No engine exists yet — `impl/rust` and `impl/go` are stubs until CLAUDE.md §11 step 5. At
that point each implementation ships a **thin harness** that: reads the manifest, filters
tiers to the flags it declares, executes each record, and compares output under the rules
above. Until then the corpus is authored and reviewed as the standing contract the first
slice must satisfy.

## 7. Open / deferred

- **Result-type assertions** — a directive to assert a result column's precise declared
  type (`int16` vs `int32`), beyond the `I`/`T`/`R` rendering tag. Deferred.
- **Integer-literal typing** — the type of a bare integer literal (e.g. is `1000` an
  `int16`, the smallest fitting type, or a context-adapted literal?) is **not yet
  specified** in [../types/](../types/). The corpus deliberately avoids entries whose
  result depends on it (implicit widening from a literal). Resolve in the type spec, then
  add coverage. This is an ambiguity the spec-first process surfaced; flag, don't guess.
- **Boolean results / connectives** — `AND`/`OR`/`NOT` over predicates arrive with the
  `boolean` type (deferred, CLAUDE.md §4). Tiers 1–3 use single predicates only.
