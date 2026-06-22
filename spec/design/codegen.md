# Codegen — design

> The reasoning behind the codegen "middle path". The generator
> ([../../scripts/gen_catalog.rb](../../scripts/gen_catalog.rb)) and the canonical data it
> reads ([../functions/catalog.toml](../functions/catalog.toml)) are authoritative; this
> doc is the *why*. When a decision here changes, change it alongside the generator and
> update [CLAUDE.md](../../CLAUDE.md) if it revises a load-bearing commitment.

CLAUDE.md §5 names **codegen the "middle path"** for large, purely mechanical surfaces —
"generate per-language stubs from the shared definition," sitting between runtime-loaded
data (portable but indirect) and hand-writing N times (drift-prone). This doc records the
decision to build it, where its boundary lies, and how it stays honest.

## 1. The decision: codegen data, hand-write logic

Spec data reaches the cores three possible ways:

1. **Runtime-loaded** — a core parses the TOML at runtime. Rejected: no shipped core may
   depend on TOML (CLAUDE.md §5), and Go/TS are pure, dependency-free (§2).
2. **Hand-mirrored + cross-checked** — each core hand-writes the table and a test asserts
   it matches the TOML. This is how scalar types and error codes work today
   ([../types/scalars.toml](../types/scalars.toml), [../errors/registry.toml](../errors/registry.toml)
   vs. `spec_constants.{rs,go,ts}`). Correct, but the per-language boilerplate grows N-fold
   with every entry.
3. **Codegen** — a build-time generator emits the per-language source. This is the middle
   path, and the right tool once a surface is large and purely mechanical.

The **function/operator catalog** is the first surface to take the codegen path: it is the
one CLAUDE.md §5 singles out ("the function catalog especially"), and it is about to grow
(arithmetic, logical connectives, `IS [NOT] DISTINCT FROM`, named functions), so the
mechanical cost is real. **Error codes have since taken the same path** (the `SqlState` enum +
its code mapping are generated from [../errors/registry.toml](../errors/registry.toml) —
[../../scripts/gen_errors.rb](../../scripts/gen_errors.rb), §5 below). Scalar types stay
hand-mirrored for now; extending the generator to them is the remaining next step (§5).

## 2. The boundary: what is generated, what is not

This is the load-bearing line, set by CLAUDE.md §5: **do NOT codegen the parser, planner,
executor, storage layer, or expression evaluator** — those are irreducibly per-language and
are where the real N-times cost (and the interesting divergence) lives.

So codegen emits **data only** — a per-language **operator descriptor table** (each
operator's `name`, `symbol`, `kind`, `arity`, `arg_families`, `arg_resolution`, `result`,
`null`, `errors`), mirroring [../functions/catalog.toml](../functions/catalog.toml):

| Generated (data) | Hand-written (logic) — consumes the data |
|---|---|
| `impl/rust/src/operators.rs` `OPERATORS` | `value.rs` `eq3/lt3/gt3`, `executor.rs` dispatch + `or3` |
| `impl/go/operators.go` `Operators` | `value.go`, `executor.go` |
| `impl/ts/src/operators.ts` `OPERATORS` | `value.ts`, `executor.ts` |
| | the `CompareOp` enum, lexer, parser symbol mapping |

The generated table describes the operators; the hand-written evaluator decides *how* to
evaluate them. A generated descriptor never contains executable comparison logic.

## 3. The mechanism

- **Generator:** [../../scripts/gen_catalog.rb](../../scripts/gen_catalog.rb) (Ruby +
  `toml-rb`, build-time only — no shipped core runs Ruby or parses TOML). Boring, explicit
  string assembly, one builder per language emitting that language's idiom (Rust
  `&'static` slices, Go gofmt-aligned structs, TS the erasable interface + typed const so
  Node type-stripping runs it unchanged). Output is deterministic (catalog order, stable
  formatting).
- **Checked-in output.** The generated files are committed and carry an
  `@generated … DO NOT EDIT` header (Go uses the canonical `// Code generated … DO NOT EDIT.`
  form). They are checked in — not generated during build — because none of the cores has a
  build step (Go and TS run from source; Rust has no `build.rs`). Regenerate with
  `rake codegen`.
- **Drift gate.** `rake verify` runs `gen_catalog.rb --check`, which regenerates in memory
  and byte-compares against the checked-in files, failing if any is stale. This is the
  guarantee that the committed source always equals the catalog.
- **Per-core cross-check.** Each core's `spec_constants` test compiles the generated table
  in and asserts it matches `catalog.toml` field-for-field. This makes the generated file a
  genuinely compiled-and-verified artifact in each language (a generated file that fails to
  compile, or drifts, fails that core's test), the same value system as the type/error
  cross-checks.

## 4. Why now, and who consumes it

Built now to establish the toolchain *before* the catalog grows — retrofitting a generator
across many hand-written entries is the cost we are avoiding. Honest about today's
consumers: the descriptor table is read by the cross-check tests, and the metadata
(`result`, `null`, `arg_families`, `arg_resolution`) is load-bearing for the **general
expression evaluator / type-checker** and the **runtime function/aggregate registry** — both
landed — which consume exactly this (operator result types and NULL behavior) as input. The
eval *logic* they dispatch to stays hand-written (§2).

## 5. Forward

- **Error codes — done** ([../../scripts/gen_errors.rb](../../scripts/gen_errors.rb)). The
  `SqlState` enum, its `code()` mapping, and an iterable `ERRORS` descriptor table are generated
  per core from [../errors/registry.toml](../errors/registry.toml) (each variant carries its
  registry `template` as a one-line doc); the hand-written `EngineError` scaffolding (message
  assembly, Display/Error rendering, the raise sites) consumes it. The same drift gate + per-core
  cross-check (§3) apply, and the generated `sqlstate.{rs,go,ts}` sit beside the hand-written
  `error.rs`/`errors.go`/`errors.ts` (which re-export `SqlState`, so consumers' import paths are
  unchanged). The boundary (§2) holds: the enum is data; nothing that *interprets* an error is
  generated.
- **Scalar types — remaining.** Extend the generator to the scalar set, replacing its hand-mirror
  (§1 option 2). Harder than errors: `scalars.toml` is ragged (integer-only / decimal-only /
  text-only fields) and `ScalarType`'s variant identity is threaded through the codec/comparator,
  so codegen emits the *attribute table* while the enum + per-variant logic stay hand-written and
  consume it by `id`. When that lands too, the impl READMEs' claim that this data arrives "via
  build-time codegen" is literally true for every table.
- As the catalog gains `arithmetic` / `logical` / `function` kinds and fields (`cost`,
  `precedence` — see [functions.md](functions.md) §6), the generator emits them with no
  schema change here: it copies whatever fields the catalog defines.
