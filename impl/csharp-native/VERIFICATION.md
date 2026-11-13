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

## Subsequent implementation checkpoints — 2026-11-12/13

The initial milestone above remains historical evidence. Dependency approval is
now granted and implemented, while full acceptance remains incomplete.

Release JIT integrated checks passed at source prefix `ad64b602` after adding:

- Exact decimal tests with 4,000 independent test-only bignum oracle trials.
- Six complete scalar encoding files (202 vectors), three complete temporal
  fixture files (319 parse/error/render vectors), and 4,000 calendar invariants.
- Nineteen packed leaf goldens, eleven interior goldens, multi-way split bytes,
  600 permuted deletions and exact tree page deltas.
- All 107 shared catalog goldens decoded/re-encoded byte-for-byte, all entry kinds.
- Recovery of 110 plaintext fixtures; failed COW stages and poisoned-handle checks.
- Database host tests for commit visibility, pending reads, rollback, retained
  cursors, uniqueness and synthetic row keys.
- Crypto provider vectors and six encrypted golden files: 309 body pages and
  their meta/key-block codecs decrypt/reseal byte-for-byte.

The provider was also exercised in earlier clean NativeAOT builds with Runtime
Async, both portable and native-instruction targets, covering scalar and AVX2
BLAKE2b paths. These earlier builds do **not** establish AOT acceptance of the
subsequently expanded SQL/database engine; a fresh full-engine gate remains due.

A complete SQL diagnostic at source
`cc0d2150e7f423cd696b795e179b02ba1eff7116522fead906ac657d447e4d5d`
passed 69/763 files (1,331 records), with 694 files failing and no capability skips.
Raw working log: `/tmp/jed-csharp-tables.log` (ephemeral). All shared SQL records
still drain the public cursor externally. No full SQL, file-host, cross-core
interchange, process, simulation, resource-accounting or benchmark acceptance is
claimed by these intermediate checks.

## Integrated native-engine checkpoint — 2026-11-13

At source SHA-256
`85011fc9fa2e593f2f9ee6e3f22c19da50fd601ed739b6d533c0a40d5a489e29`,
the expanded native engine passes:

| Command | Result |
|---|---|
| `mise run csharp-native:test` | Release JIT full host/fixture/supplement checks pass; AVX2 enabled |
| `mise run csharp-native:crypto --portable` | Provider vectors pass with AVX2 disabled |
| `mise run csharp-native:aot` | Clean SDK-default instruction-set NativeAOT build and full host checks pass; AVX2 disabled |
| `mise run csharp-native:aot --runtime-async --native-instructions` | Clean Runtime Async NativeAOT build and full host checks pass; host-specific instructions, AVX2 enabled |
| `mise run csharp-native:codegen --check` | Eight generated data catalogs match the pinned spec |
| `mise run verify` | Normal repository spec checks pass |

Both NativeAOT builds completed without warnings or errors. These results cover
implemented host and fixture paths, including exact numerics, temporal/timezone
values, recursive codecs, JSON binary goldens, prepared AST reuse, real file I/O,
encrypted storage, compressed/overflow values and process coordination. They do
not establish complete SQL, resource, simulation, maintenance or platform acceptance.

The expanded Go↔C# owner/client interoperability run passed separately at source
`e3140dd7f61f56a203f32e8125ea7fded2c8068e8862420a9d2a16cfd9950c6a`.
It covers both owner directions, plaintext and encrypted files, role handoff,
owner death, lost-acknowledgment recovery, snapshot pins, and compressed and
incompressible large SQL values. Its source identity differs from this checkpoint;
these are scoped interoperability checks, not the entire shared process corpus.

The first full-SQL run at that same `85011fc9` source aborted with a stack
overflow in sequence-write classification while expanding views through CTE
shadowing. It is an incomplete diagnostic, not a 763-file result. The host/AOT
checks above did not cover that SQL case; a corrected source needs fresh checks.

The corrected sequence-write discovery carries lexical CTE scope, uses a fresh
scope when entering a view, guards active view cycles and bounds expansion.
At corrected source SHA-256
`8d5b2a68950da35d5b73653205a281f9afb30c0b1e17bd6574e1d9db6a047d8c`,
`mise run csharp-native:conformance --all` completed **763 files**: **293 passed**,
**7,148 records in passing files**, **470 failed**. The command exits nonzero for
those failures. No unsupported capability is silently skipped. Ephemeral raw log:
`/tmp/jed-csharp-checkpoint-full2.log`.

After that complete SQL diagnostic, all four integrated host/crypto/build commands
listed above were rerun successfully at corrected digest `8d5b2a68…`: Release
JIT, portable provider vectors, SDK-default NativeAOT, and Runtime Async NativeAOT
with host-native instructions. Both clean NativeAOT publishes again completed
without warnings or errors. This verifies the corrected checkpoint's implemented
host paths while leaving the 470 SQL failures and remaining acceptance gates open.
