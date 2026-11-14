# Java experiment tooling

These tasks are opt-in and do not change the supported-core task graph. Run them
from the checkout through `mise run java-native:<task>`.

- `setup` downloads the exact JDKs and Bouncy Castle binary/source JARs declared in
  `toolchains.json`, verifies SHA-256 before extraction, and installs under ignored
  `.dev/java-native`. `--downloads DIRECTORY` uses local artifacts with the same
  checksum checks. No system Java installation, Maven, Gradle, or sudo is needed.
- `inventory` extracts the immutable baseline spec from local Git objects and
  reports all expected files. `--output PATH` writes every file's SHA-256. This
  reports an acceptance target, never a claim of implementation coverage.
- `codegen` emits mechanical catalogs from the frozen spec. `--check` verifies
  the checked-in output. No parser, expression, storage, or execution code is
  generated.
- `build` compiles main and test code with plain `javac` and packages `jed.jar`.
  The `stable` variant uses Temurin 25; `ea` uses JDK 28 with the same Java 25
  source level; `valhalla` requires explicit preview replacement sources under
  `src/valhalla/java`, uses source level 28, and otherwise fails closed.
- `check` invokes explicit host/invariant tests using the frozen byte fixtures.
- `query 'SQL'` drains a public cursor and reports names, types, values, and cost.
- `conformance --all`, `--suite NAME`, or `--file PATH` attempts complete files.
  Recognized but unimplemented harness directives fail; capability requirements
  never suppress SQL records. An unsupported durable/encrypted host mode fails
  explicitly. Passing a selected file does not imply full parity.
- `benchmark --profile smoke --retained-memory` checks 24 matched workloads across
  all three Java variants and the frozen Go/Rust peers. It builds peers offline
  from local Git objects and existing dependency caches. Java variants compile
  one captured source snapshot. Every actor must match result checksums, row
  counts, and execution costs. Smoke uses 64 input rows and is correctness evidence.
- `benchmark --profile diagnostic --repeats 3 --data-directory PATH` uses 4096
  rows and bounded warmup: 5,000 executions for cheap queries, 200 for other
  queries, and 100 for transaction/open/parallel cases. Run it on an otherwise
  idle host. Early/late measured medians and Java compilation-time deltas expose
  remaining startup or tier-compilation effects; a fixed warmup count alone
  does not establish steady state. Evidence retains
  startup, warmup and execution samples, allocation/GC counters where available,
  process peak RSS, filesystem identity and artifact hashes. All actors use
  256 MiB cache/work-memory budgets and synchronous file commits. Actor order
  rotates between repetitions. Parallel reads include platform-thread launch,
  separate session/preparation, and 64 queries per each of four workers.
  `--retained-memory` adds separate Java processes with explicit GC requests;
  their timings are excluded from comparisons. A GC request is not a heap cap.
- `profile --evidence PATH --variant stable` records JFR for selected workloads
  using an immutable benchmark actor, then saves the recording, event summary,
  input and hashes. `--cases`, `--warmup`, and `--iterations` control the probe;
  `--rows` changes the generated setup size while retaining the original queries.
  New captures include `org.jed.BenchmarkPhase` setup/warmup/measured events.
  Profiled timings are not comparison evidence. Allocation events are samples.

Benchmark output stays under `.dev/java-native/benchmarks/` by default. Database
files may be placed on another filesystem with `--data-directory`; evidence
records the actual mount. Cold-open cases include open/prepare/query/drain/close
with a warm OS cache. Result rendering and checksums are timed. Java allocation
counters distinguish the calling thread from parallel workers; Go's counter is
process-wide, and Rust's allocation/GC counters are unavailable. Native/JIT memory
is outside Java heap-retention measurements. Tmpfs commit timings do not measure
physical-disk durability latency.

Each build has a unique immutable directory under `.dev/java-native/build/`, so
concurrent tasks cannot replace files between compilation and actor capture. Sources,
resources, and tooling are copied into that directory before compilation; the
source digest describes those exact captured bytes.
Build metadata records the source digest, runtime, preview setting, and dependency
and JAR hashes. Check/query/conformance executions copy their actors and dependency
into a separate `.dev/java-native/actors/` directory with an artifact manifest so
later builds cannot replace the actor during a run. No benchmark performance claim
is made by these build or functional check tasks.

Bouncy Castle is approved only for Argon2id and its internal helpers. Putting its
JAR on the experiment class path does not authorize its other algorithms or
registering it as a general JCA provider. The source JAR is installed for review.
