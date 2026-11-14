# Native Java core experiment

Authorized 2026-11-14. This charter records the maintainer's approval of all
seven recommendations in the Java readiness investigation, including local
toolchain/dependency setup, parallel implementation and independent review.
The frozen experiment is **complete and remains experimental**. All 42 correctness
gates pass on source
`419675aa4fa4b2afeaa9a645d6ebc3e68e38579e77858bb1816e12dd2848c5fc`.
Matched measurements cover 24 workloads, five actors and three process repeats,
plus separate Java memory probes. Shared-host load and bounded warmup limit the
timing conclusions; they do not establish performance parity or promotion. See
`impl/java-native/VERIFICATION.md` and `impl/java-native/BENCHMARKS.md` for evidence.

## Baseline and lifecycle

`impl/java-native` is an independent handwritten Java engine targeting the
supported-core specification at `3e5962f9affc06c768df1688765162a65a893848`, format
v53. The language-neutral specification and corpus are canonical; existing
cores provide implementation patterns, not authority. The target is full
implemented baseline parity, not a scalar or in-memory subset.

Only explicit `java-native:*` mise tasks install, build, generate, verify or
benchmark Java. Default setup, tests and CI do not depend on Java. The scope is
Linux x86-64; other platforms remain unverified. Freeze actual source and artifact
identities after full verification and measurement. Future supported-core changes
do not automatically update this baseline. Promotion, package publication,
website integration, JDBC/ORM/reactive integrations, GraalVM Native Image and
other platforms require separate decisions. Local commits are authorized;
pushing and master integration are not part of this experiment.

## Toolchains and comparisons

The approved stable target is Temurin 25.0.5+8 without preview features. Select
and checksum-pin one JDK 28 early-access build during setup. Download both into
this checkout's ignored development directory; record upstream provenance and
actual versions. Missing approved artifacts must be reported, not silently
substituted. No reference-source provisioning is authorized.

Compare the stable engine on Java 25, ordinary classes on JDK 28, and selected
value-class conversions on the same JDK 28 build. Keep one semantic engine;
the preview variant must not acquire different SQL, cost or storage behavior.
Value-object layout and scalarization are optimizations, not conformance facts.
Keep cold startup, warmup and steady-state results distinct. Isolate and report
preview compiler/runtime defects without preventing stable-core completion;
never describe an unverified preview variant as accepted.

## Implementation and public API

Parser, static resolver, planner, executor, storage, recursive type behavior,
exact arithmetic and noncrypto built-ins are handwritten Java. Large mechanical
catalogs may be generated from shared data by opt-in Ruby tooling. No transpiled
engine or call to another engine is allowed. SQL decimals use handwritten limbs,
not BigDecimal or BigInteger as the value-path implementation.

Prefer primitive numeric arrays, separate null maps, packed byte pages and
concrete typed methods in measured hot paths. Immutable objects are appropriate
for types and plans. Do not require off-heap storage or preview features for the
stable core, and do not assume the whole database is resident. Table execution
must use the pager/packed-page seam from the first storage slice.

The public embedding API is blocking and streaming: ordinary methods, explicit
cooperative cancellation, prepared statements, owned rows, AutoCloseable resource
lifetimes and native parameter/result conversions. It is usable from ordinary
or virtual threads; neither implies faster CPU execution or nonblocking file I/O.
Database handles support concurrent sessions. Sessions/cursors reject overlapping
operations; sequential use may move between threads. Cancellation preserves the
shared accounting, transaction and commit-outcome contracts; blindly interrupting
a thread must not close a shared file channel. Host functions execute in process
and support the existing batch contract. No mandatory executor, callback RPC or
CompletableFuture wrapper is required. `Rows.affectedRows()` and the drained
`Session.execute` result use `-1` when the statement has no affected-row count;
zero is a real count for zero-row DML and ANALYZE. CTAS with data reports its
inserted rows; SELECT, ordinary DDL and transaction control have no count.

Host strings reject ill-formed UTF-16; SQL text semantics use the shared UTF-8,
Unicode-scalar and pinned collation/timezone contracts. Integer overflow, exact
decimals, floating values, NULL, errors and resource charges follow the shared
specification rather than Java defaults. JVM allocation counts never determine
logical resource costs.

## Approved host and crypto boundaries

Narrow JDK Foreign Function API calls may implement Linux flock, directory
durability, atomic filesystem operations and other required OS host behavior.
Their ownership, native-access configuration and mixed-core lock behavior require
tests. This is not permission for Unsafe engine access, native engine logic, JNA
or an unrelated native dependency. FileChannel's fcntl locks cannot replace the
protocol-v2 flock contract.

The maintainer approved `org.bouncycastle:bcprov-jdk18on:1.86` solely for Argon2id
and its necessary internal helpers, subject to source review, verified artifact
pinning and shared vectors. Use JDK providers for MD5, SHA-224/256/384/512, AES
blocks, AES-GCM and HKDF. XAES is the shared derivation glue over approved AES,
not a new primitive. Validate credential bounds before provider allocation;
record secret-erasure limits. Additional packages require explicit approval.
The initial harness uses existing Ruby infrastructure and JDK tools without a
new test/benchmark framework.

## Frozen page-service read-bound compatibility

The Java owner requires a connection-held read or writer pin for `read_pages`,
rejects meta indices, and bounds indices by the **current committed** page count.
This deliberately follows the observed frozen Go, Rust and TypeScript owner
behavior, rather than the narrower wording “below the pinned snapshot’s
`page_count`” in `spec/pageservice/README.md` §3.3. It does not revise that shared
specification. At the frozen revision, Go `psServerConn.pinnedPageCount`, Rust
`Shared::page_service_page_count`, and TypeScript `OwnerConnection.readPages`
all return the current committed bound after checking the connection's pin. Go
and TypeScript explain that a pin blocks compaction and the page count grows
between compactions. The retained reader watermark also prevents pages needed
by that reader from being reused; a newer committed page being readable does
not change the reader's logical roots or authorize writes.

A strict original-snapshot bound breaks an ordinary frozen Go client: after
`commit_end` releases its writer pin, `Session.publish` calls
`snapshot.demoteCleanLeaves`; `tableStore.demoteCleanLeaves` faults a newly
committed leaf through `sharedPaging.faultLeaf`. The connection names its newest
remaining pin, which can belong to an older reader. In the diagnostic case,
this requested page 9 under txid 3, whose original page count was 6. This is a
post-commit residency operation, not an old query adopting new logical data.
The experiment's mixed-process test covers both ownership directions and both
plaintext and encrypted files, with alternating writes and held old readers.
The owner host test separately rejects reads without a held pin and indices
at or above the current committed high-water. The complete page-service matrix
now passes on all three variants; immutable evidence is recorded in
`impl/java-native/VERIFICATION.md`.

## Acceptance and autonomy

All applicable baseline SQL files must pass memory/plain/encrypted inventories,
including exact values, types, errors, cost and plans. Harnesses drain public
cursors externally and check final cost and drain-time errors. Unimplemented
capabilities fail full runs; they are never silently skipped or given changed
expected answers. Canonical mode exclusions remain explicit.

Acceptance also requires byte fixtures, bidirectional supported-core interchange,
resource/spill/cancellation and host API checks, mixed-core process scenarios,
shared concurrency schedules and threaded stress, saved/generated deterministic
failure/recovery traces, applicable PostgreSQL oracle verification and healthy
default tasks without Java. Scope and host matrices record every outstanding
gate; a focused test result does not certify full parity.

Profile and optimize after correctness, with repeated matched workloads against
pinned Go and Rust actors. Record startup/warmup, allocations, GC, retained memory,
concurrent reads, real durable writes where available, checksum/cost agreement,
hardware/filesystem/runtime settings and raw evidence. Tmpfs barriers are not
physical-disk durability latency. Competitive performance is an objective, not
a promised ratio. Finish when significant gaps are explained and reasonable
correctness-preserving measured improvements are exhausted.

The maintainer authorized development-stack setup, long verification and benchmark
runs, local checkpoint commits, parallel implementation and independent review.
Routine module boundaries, API naming, algorithms, profiling decisions and fixes
are delegated within this charter. New dependencies, irreconcilable spec conflicts
or necessary scope changes must be surfaced; ordinary implementation work does not
require repeated approval.

`impl/java-native/PROGRESS.md`, `VERIFICATION.md` and `BENCHMARKS.md` record progress,
executed evidence and measured results respectively. None of the acceptance gates
is certified by this charter alone.

## Experimental storage budget API

`Database.memory(long maxStorageBytes)` and
`Database.memory(Environment, int pageSize, long maxStorageBytes)` configure a
memory domain. File creation/open uses
`FileOptions.Builder.maxStorageBytes(long)`. A handle exposes
`maxStorageBytes()`, `setMaxStorageBytes(long)` and `storageBytes()`; nonpositive
limits mean unlimited and limits are not persisted. The main memory domain uses
real packed pages and the RAM free list; a file reports its persisted live-page
count, excluding metadata, dead pages and free-list/manifest storage. Admission
runs before writes and uses `54P06`, as in memory.md §8. Attachment-qualified
forms are implemented alongside attachment support; their existence does not
certify that slice before its separately recorded verification.

The experimental Java host surface now includes `Database.read()` for a pinned
read handle, `Database.openRemote(identity, PageTransport, FileOptions)` for a
host-provided authenticated duplex stream, and `PageRelay.serve` for its serving
end. The core opens no network listener; a host may supply SSH stdio. Credentials
travel in the bounded hello message and are removed before forwarding to a local
owner. `FileOptions.synchronous(false)` selects the shared validated-COW batch
contract; `flushIntervalMillis` defaults to 1000. The explicit local `flush()`
method, close and ownership release use the ordinary durable barrier.

`Database.compact` and `Database.rekey` use a bounded fresh-image rewrite and
atomic replacement. A plaintext handle must opt into
`FileOptions.protectedScratch(true)` before encrypting its database; encrypted
handles protect scratch automatically. Data-key rotation re-encodes rows into
the target page layout through bounded authenticated scratch. These additions
remain opt-in experiment APIs and do not establish production delivery or
promotion. Exact acceptance checkpoints are recorded in the experiment's
verification ledger; saved simulation acceptance remains incomplete.
