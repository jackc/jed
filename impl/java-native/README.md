# Native Java experiment

An independent, handwritten Java core targeting the supported-core specification
at `3e5962f9affc06c768df1688765162a65a893848`, format v53. All 42 correctness
acceptance gates pass on the recorded frozen source. It remains opt-in;
promotion and production packaging require separate
decisions. No normal test, benchmark, setup or CI task depends on Java.

The stable target is Java 25 without preview features. Two comparison variants
use pinned OpenJDK 28 EA24: unchanged ordinary classes, and explicit value-class
replacements for `SqlValue`, `SqlUuid` and `SqlInterval`. The build verifies that
the replacements differ only in the `value` modifier. The preview is optional;
the engine does not depend on it for SQL or storage behavior.

See the [charter](../../spec/design/java-native-experiment.md),
[scope and host matrix](../../spec/design/java-native-scope.md),
[verification evidence](VERIFICATION.md), [measurements](BENCHMARKS.md), and
[host/crypto review](HOST_REVIEW.md). Verification covers SQL, storage, recovery,
host APIs, process coordination and concurrency. Performance measurements and
their limits are recorded separately from correctness acceptance.

## Build and verify explicitly

```sh
mise run java-native:setup
mise run java-native:codegen --check
mise run java-native:build
mise run java-native:check
mise run java-native:conformance --all
mise run java-native:conformance --all --disk
mise run java-native:conformance --all --encrypted
mise run java-native:stress
mise run java-native:process --cores java-native,go,rust,node --evidence-dir .dev/java-native/process-evidence
mise run java-native:acceptance --evidence-dir .dev/java-native/acceptance-evidence
```

Setup installs checksum-pinned JDKs and the approved Bouncy Castle binary/source
JARs into the checkout's ignored `.dev/java-native/` directory. No system Java,
Maven, Gradle, OpenSSL installation or native engine library is needed. The
existing repository Ruby/Go/Rust tools support verification and peer comparisons.
`--variant ea` and `--variant valhalla` select the comparison builds. Build output
prints the JAR directory and immutable source digest; conformance/check tasks
also capture source, classes, JAR and dependency identities for replay.

Conformance accepts `--all`, `--suite name`, `--file path` or `--files path,path`.
The file modes reopen before every applicable record. Shared mode exclusions
remain explicit. Encrypted runs first execute a frozen independent Go engine,
then assert every cost-bearing result and EXPLAIN against its encrypted output;
all other values, types and errors remain checked against the corpus. Missing,
changed or incomplete peer evidence fails the run. `--actor directory` replays
a verified immutable actor instead of rebuilding.

The process task runs all selected owner/client pairings against the 31 frozen
page-service scenarios. Optional Go, Rust and Node peers are exported from the
same baseline and built using existing offline dependency caches. Node requires
the installed repository packages to match the frozen package lock. An evidence
directory preserves actor identities and final file images for byte comparison.

The acceptance task freezes one source for all three Java variants and runs the
SQL inventories, host checks, threaded stress, mixed processes, 19 saved fault
traces, 66 generated traces and persisted review regressions. Its report retains
each gate's result and log hash. Parallel acceptance runs provide correctness
evidence; run performance measurements separately on an otherwise idle host.

## Embedding surface

The API is blocking and streaming. A `Database` shares committed state across
sessions; a session or cursor rejects overlapping operations. Sequential use
may move between ordinary or virtual threads. Rows own their values and survive
cursor closure. Always drain or close a cursor; execution errors and final cost
can arrive while draining.

```java
import java.nio.file.Path;
import org.jed.*;

try (Database db = Database.create(Path.of("example.jed"), FileOptions.defaults());
     Session session = db.session()) {
    session.execute("CREATE TABLE item (id i32 PRIMARY KEY, label text)");
    session.execute("INSERT INTO item VALUES (1, 'example')");
    PreparedStatement query = session.prepare("SELECT label FROM item WHERE id = $1");
    try (Rows rows = session.queryPrepared(query, SqlValue.ofInteger(SqlType.I32, 1))) {
        Row row;
        while ((row = rows.next()) != null) {
            System.out.println(session.render(row.get("label")));
        }
        System.out.println("cost=" + rows.cost());
    }
}
```

Use `Database.open` for an existing file and `Database.memory` for a memory
database. The classpath distribution requires the generated `jed.jar` and the
pinned `bcprov-jdk18on-1.86.jar`. Linux file coordination uses the JDK Foreign
Function API; launch file-host applications with
`--enable-native-access=ALL-UNNAMED`. The preview build additionally requires
`--enable-preview` on its pinned JDK 28 runtime. The pure memory core does not
initialize native file coordination.

Explicit transactions use SQL `BEGIN`, `COMMIT` and `ROLLBACK`. `Database.read()`
opens a pinned read handle. `Session.execute` drains internally and returns an
affected-row count, or `-1` when the statement has no such count. Prepared query
overloads accept a cooperative `Cancellation` signal; cancellation does not
interrupt a thread or close a shared file channel. Host environment construction,
function registration and byte transport stay outside built-in SQL.
