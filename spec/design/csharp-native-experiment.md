# Native C# core experiment

Authorized 2026-11-12; completed and frozen 2026-11-13. This is an **opt-in
experimental core**, not a supported core or a production delivery decision. The semantic baseline is the
supported Rust/Go/TypeScript surface at
`b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. The language-neutral specification,
catalogs and corpus at that revision are authoritative. Existing engines supply
implementation patterns, never a reference implementation. Swift's older baseline
and Ruby's frozen coverage do not narrow this experiment.

## Lifecycle and completion

`impl/csharp-native` contains an independent handwritten C# engine. It may not
invoke another engine, execute SQL in subprocesses, or transpile engine code.
Only explicit `csharp-native:*` mise tasks build, generate, test or benchmark it.
Normal tooling must not require .NET. Future supported-core changes do not
automatically update the baseline. Freeze only after recording actual coverage,
omissions, verification actors and measurements; promotion requires a separate
maintainer decision. No website integration or package publication is included.

Full baseline parity is the endpoint. The [scope matrix](csharp-native-scope.md)
and [host matrix](csharp-native-host-matrix.md) track missing work explicitly.
Passing a selected slice does not establish a capability or whole-core parity.
Unimplemented records must fail an explicitly requested full run; they must not
be silently skipped, assigned new expected answers, or reported as passing.

Acceptance includes shared SQL, exact types/errors/costs, byte fixtures,
bidirectional plain/encrypted interchange with the supported cores, resource and
cancellation boundaries, Linux multi-process interoperability, deterministic
failure/recovery simulation, PostgreSQL oracle checks and healthy default tasks
without .NET. Harnesses drain public cursors externally and check final costs and
drain-time errors. Internal collectors cannot substitute for that path.

## Recorded completion

Engine34 passes all applicable frozen SQL files across memory/plain/encrypted
storage and default/runtime-async JIT/NativeAOT: 12 inventories with exact
cross-variant and independent encrypted cost/EXPLAIN checks. All 763 distinct SQL
files pass in at least one storage mode. All four host executables, 496 mixed-core
process cases and 16,000 threaded snapshot checks pass, including the final-review
writer-pin admission cleanup regression.

The stable final fault sweep passes 85 traces: 76 exact peer comparisons and nine
intrinsic fault checks under the canonical per-profile policy. Four compilation
variants each completed three sequential diagnostic measurement runs. See the
[verification record](../../impl/csharp-native/VERIFICATION.md) for source/artifact
identities and freeze proof, and [measurements](../../impl/csharp-native/BENCHMARKS.md)
for results and limits. Current time/allocation overhead against Go is substantial;
the tmpfs diagnostics do not validate earlier near-Rust performance expectations
or physical-disk durability latency. Production delivery remains undecided.

## Toolchain and platforms

Initial acceptance targets Linux x86-64. Pin .NET 11 SDK **11.0.100** in the
package's `global.json`; the explicit setup task verifies Microsoft's SHA-512
before extracting the SDK into this checkout's ignored `.dev/` directory.
Normal repository setup does not install it. Windows, macOS, ARM, browser and
mobile hosts remain unverified until separately exercised.

The maintainer requested considering modern async C# and .NET 11 on 2026-11-12.
.NET 11 reached GA on 2026-11-10; choose it for this forward-looking experiment.
Its STS lifecycle is a later packaging consideration, not a reason to maintain
two framework targets now. The runtime requires x86-64-v2 on x64. Record that
requirement with benchmark hardware. See Microsoft's [release announcement](https://devblogs.microsoft.com/dotnet/announcing-dotnet-11/)
and [runtime notes](https://learn.microsoft.com/en-us/dotnet/core/whats-new/dotnet-11/runtime).

The engine is managed C#, independent of whether its host uses JIT or NativeAOT.
Keep dynamic code generation and reflection-dependent engine behavior out of the
design. AOT compatibility is a gate to prove, not a property inferred from a
project flag. Verify and measure a release JIT host and a NativeAOT host separately
before freezing, recording runtime, warmup, GC configuration, startup, allocations,
durability settings, checksums and exact actor identities. Existing performance
expectations in cores.md are hypotheses, not measured C# results.

## Implementation boundaries

Use flat modules and explicit stages: lexer/parser, static resolution, execution,
resource accounting and storage. Shared catalog generation emits data only and
is opt-in. No generated parser, planner, executor or recursive type behavior.
The initial vertical slice is FROM-less scalar SELECT; it must not grow into an
eager in-memory database substitute. Table support must use the shared page and
packed-record contracts from its first storage slice.

SQL integers use checked width-specific arithmetic, with specified exceptions
such as MIN % -1 = 0. SQL NULL is separate from false. Text is compared as UTF-8,
never using culture-sensitive or UTF-16 ordering. Host string input must reject
ill-formed surrogate sequences. Unicode kernels count scalars. Names, UUID bytes,
temporal values and float rendering follow the spec rather than host defaults.

SQL decimal uses handwritten limbs and the exact scale/rounding contracts;
neither System.Decimal nor System.Numerics.BigInteger is its value-path engine.
Open types derive recursively from their descriptors. Resource charges use the
shared logical schedules, never CLR allocation estimates. Safe spans and bounded
buffers are permitted; unsafe engine memory access is not.

The embedding surface is **async-first**: QueryAsync, async row iteration,
CancellationToken and IAsyncDisposable. Await actual page-device and transport
operations, including open/recovery and commit/maintenance stages. Resident
evaluation stays synchronous inside one semantic core and may complete a
ValueTask immediately; no Task.Run wrapper, sync-over-async blocking, mandatory
dedicated execution worker, or per-expression async allocation. Public operations
use the task-based asynchronous pattern; repeated cursor advancement can use
ValueTask when it commonly completes synchronously. Keep synchronous entry points
internal for conformance and diagnostics until a separate public API decision.
Async suspension does not grant concurrent use of a session or cursor: reject
overlapping operations and permit sequential continuation on different threads.
Database handles provide the shared thread-safe ownership boundary.

.NET 11 Runtime Async is a compiler/runtime optimization to verify separately
with identical conformance, not an execution-semantic dependency. First use stable
async APIs without enabling preview language features. Evaluate runtime-async
compilation and NativeAOT on the same source; neither changes SQL scheduling,
error/cost order, cancellation boundaries or commit outcome semantics.
Host functions use in-process delegates and the existing batched contract.
ADO.NET, EF integration and public NuGet packaging are separate follow-ons.

Crypto and ordinary OS operations stay behind narrow host seams. The maintainer
approved BouncyCastle.Cryptography 2.7.0 for SHA-224 and Argon2id with their internal
helpers on 2026-11-12; see the [crypto audit](csharp-native-crypto.md).
Protocol-v2 locks, directory durability and file replacement need explicit host
proofs. BCL method names alone do not prove cross-core lock interoperability.

## Records

`impl/csharp-native/PROGRESS.md` records work and unresolved gates;
`VERIFICATION.md` records executed checks and their scope. `baseline.json` pins
the original revision. Any supplemental corpus files are identified separately;
they do not silently move that revision. Implementation and performance evidence
must identify the actual source tested. No freeze or performance claim is implied
by this charter.

## Historical full-build readiness audit (2026-11-12)

The scalar milestone is committed locally as `aeb741bd`. The one identified
external approval blocker was resolved by the maintainer's approval of the
concrete crypto dependency in the [crypto audit](csharp-native-crypto.md).
Subagent implementation/review was also explicitly authorized on 2026-11-12.

The SDK, compiler, Linux host, PostgreSQL oracle, supported peer builds and shared
fixtures are available in the current checkout. Unicode/time-zone bundles and
regex vectors are already repository inputs; implementing their consumers needs
no new package or reference-source provisioning. Safe, narrow OS interop for
flock, directory durability and other sanctioned Linux host operations is within
the existing host-seam scope. Its cross-core behavior still requires proof.

The remaining work is implementation and verification, not a sequence of user
design decisions. Proceed with the existing choices: full frozen baseline,
Linux x86-64, .NET 11, async-first public APIs, handwritten independent engine,
JIT and NativeAOT acceptance, opt-in tasks and no production packaging. Build
storage/encoding and the full harness early; then complete types and expressions,
catalog/transactions and DDL/DML, relational planning/execution, resource/spill
behavior, encryption/maintenance and the process/simulation adapters. Integrate
each slice against complete relevant shared files and host fixtures, followed by
the full gates in the scope/host matrices and diagnostic benchmarks.

Routine architecture, module boundaries, API naming, tests and defect fixes can
be resolved within this charter. A future spec contradiction, additional
dependency or necessary scope change must be surfaced explicitly. Such an
unknown is not evidence of a present blocker. Completion means verified baseline
parity and recorded measurements; passing the scalar slice is not completion.
