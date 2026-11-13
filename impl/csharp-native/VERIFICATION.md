# C# experiment — initial verification

Date: 2026-11-12 (America/Chicago). **First scalar milestone only; the engine is
incomplete and not frozen.** No performance claim or package publication.

## Identity

- Semantic baseline and supported peer source: `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`.
- C# implementation: tested as working-tree additions, subsequently committed
  locally as `aeb741bd` on `codex/csharp-native-experiment`.
- Source SHA-256: `35137d5de128ab1d4ae8a4813da3a0b7dda504320bb8a5c93ac087cb511e647d`.
  The task launcher computes this from sorted source paths and bytes, including
  engine/harness C#, project configuration, generated data, task tooling and the
  supplemental corpus. Documentation and ignored build outputs are excluded.
- Supplemental corpus SHA-256:
  `899b1c3b4ed47895148763fed4d31cd690076aef48ead06796f22166cd8b745c`.
- Toolchain: Microsoft .NET SDK 11.0.100, net11.0; Linux x86-64,
  Ubuntu 26.04.1, kernel `7.0.0-38-generic`.

## Passing checks

| Command | Result and scope |
|---|---|
| `mise run csharp-native:test -c Debug` | Zero build warnings/errors; host/invariant tests, negative adapter tests and all 45 supplement records pass |
| `mise run csharp-native:test` | Same checks pass in Release with default async compilation |
| `mise run csharp-native:test --runtime-async` | Same checks pass with `Features=runtime-async=on`, from a fresh compilation |
| `mise run csharp-native:aot --runtime-async` | Clean Linux NativeAOT publish; the native executable passes the same checks |
| `mise run csharp-native:codegen --check` | Four generated data catalogs match the pinned spec |
| `JED_CONFORMANCE_FILTER=query/scalar_foundations.test mise run conformance:rust` | Entire supplement passes in memory and disk modes |
| `JED_CONFORMANCE_FILTER=query/scalar_foundations.test mise run conformance:go` | Entire supplement passes in memory and disk modes |
| `JED_CONFORMANCE_FILTER=query/scalar_foundations.test mise run conformance:ts` | Entire supplement passes in memory and disk modes |
| `mise run corpus:check spec/conformance/suites/query/scalar_foundations.test` | Byte-identical regeneration against the checkout's PG 18 oracle, with four existing planning-time divergences explicitly ledgered |
| `mise run verify` | All normal shared spec/catalog/fixture checks pass; no .NET dependency added to this task |
| `git diff --check` | No whitespace errors |

The host tests cover inline async completion, owned returned values, drain-time
errors and partial costs, per-statement and lifetime limits, canceled Task and
ValueTask state with SQLSTATE `57014`, invalid host Unicode, nonnullable reads,
deep input rejection, the 256-depth boundary, UTF-8 identifier length, disposal
without evaluation and locale-independent names/rendering. They do not establish
future asynchronous I/O, prepared handles, parallel reads or thread safety of an
unimplemented database.

Negative adapter checks reject unsupported directives and conditionals, incorrect
types/costs/error codes/row multiplicity, dangling directives and errors during
cursor drain. Selected SQL files run in full; the harness never replaces a cursor
drain with an engine-owned collector.

## Full-baseline attempt — expected failure, not acceptance

`mise run csharp-native:conformance --all` attempted all **763** pinned SQL files:
**1 passed, 762 failed**, with no capability-based skips. The passing file was
`cast/boolean_prefixes.test` (5 records). The ignored raw report is
`results/baseline-attempt.log`. Failures include missing table/relational/type
support and unimplemented harness formats/directives; the conservative adapter
also rejects prose comments shaped like unknown directives. These failures are
part of the initial inventory, not regressions waived to claim parity.

The 45-record supplement is separate from that baseline; it does not move the
pinned semantic revision. Most SQL, storage and acceptance work remains missing.
No file images, encryption, process pairs, simulation traces, resource memory
limits or benchmarks have been accepted for C#.

## Corrections and limits

The initial ConfigureAwait call was ambiguous because Rows implements both
IAsyncEnumerable and IAsyncDisposable; the harness now selects enumeration
explicitly. Review corrected WHERE/OFFSET order, nesting accounting, async
cancellation state and admission-failure ownership before the final tests.
The AOT task initially tried cleaning a RID-specific build after a normal restore;
it now restores the matching SDK packs before cleaning and publishing. Reported
AOT success belongs to that corrected task.

The four oracle overrides preserve supported jed semantics around skipped
projections and NULL filters versus PostgreSQL's planning-time constant folding.
They are not new C# divergences. The full default CI suite was not run: no
supported engine implementation changed, and scoped three-core conformance plus
normal spec verification passed. Windows, macOS, ARM and browser/mobile hosts
remain unverified. Crypto provider selection and dependency approval remain open.
