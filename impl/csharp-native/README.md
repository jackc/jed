# Native C# experiment

An independent, handwritten C# engine experiment targeting **.NET 11** and Linux
x86-64, completed and frozen against semantic baseline
`b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. It remains opt-in; production delivery
and supported-core promotion require separate decisions.

All 763 distinct baseline SQL files pass across their applicable storage modes.
Engine34 passes all 12 memory/plain/encrypted inventories under default/runtime-async
JIT and NativeAOT, all four host executables, 496 mixed-core process cases and
16,000 threaded snapshot checks. The final stable simulation sweep passes all
85 traces: 76 exact peer comparisons and nine intrinsic fault checks. See the
[charter](../../spec/design/csharp-native-experiment.md),
[scope](../../spec/design/csharp-native-scope.md) and
[verification record](VERIFICATION.md) for identities, exclusions and freeze proof.

[Measured diagnostics](BENCHMARKS.md) show substantial current time and allocation
overhead against pinned Go. They do not validate the earlier near-Rust performance
expectations. Files were on tmpfs: synchronous barriers were enabled, but commit
timings are not physical-disk durability latency. These results do not select a
production C# delivery strategy.

## Run explicitly

```sh
mise run csharp-native:setup
mise run csharp-native:restore
mise run csharp-native:codegen --check
mise run csharp-native:test
mise run csharp-native:test --runtime-async
mise run csharp-native:aot --runtime-async
mise run csharp-native:inventory
```

Setup downloads and SHA-512-verifies Microsoft SDK 11.0.100 into this checkout's
ignored `.dev/csharp-native/` directory. `--archive /absolute/path/to/sdk.tar.gz`
uses an existing download with the same checksum check. Restore uses the committed
package locks and source mapping for the approved BouncyCastle.Cryptography 2.7.0
SHA-224/Argon2id dependency. The explicit AOT task also restores the SDK's Microsoft
compiler/runtime packs and needs a native compiler such as clang.

`csharp-native:conformance --file path/to/file.test` attempts one complete file.
`--suite name` attempts a pinned suite; `--all` attempts the pinned baseline.
Neither option silently skips unsupported records. `--variant aot` runs the
published NativeAOT executable; `jit-runtime-async` and `aot-runtime-async`
select the separately verified runtime-async compilations.
`--disk` reopens the native file and session before every eligible record. `--encrypted`
uses the same reopened file path under a fixed fixture credential and writes the separate
cost/EXPLAIN transcript; `csharp-native:conformance-peer <run-log>` compares complete passing
files against an independently executed pinned Go transcript. Explicit shared mode exclusions
are reported as `SKIP`; unsupported SQL still fails. The default selection is the scalar foundation supplement.
No normal repository task builds or installs this experiment.

## Async embedding

```csharp
using Jed;

await using var database = await Database.CreateAsync(new() { Path = "example.jed" });
await using var session = database.CreateSession();
await using var rows = await session.QueryAsync(
    "SELECT CAST(40 AS i32) + 2 AS answer",
    maxCost: 100,
    cancellationToken: cancellationToken);

await foreach (var row in rows.WithCancellation(cancellationToken))
    Console.WriteLine(row[0].AsInteger());

Console.WriteLine(rows.Cost);
```

`QueryAsync` resolves the statement and returns a streaming cursor; evaluation
errors can occur during iteration. Values are owned and remain valid after the
cursor advances or closes. `Rows.ReadAsync` exposes a `ValueTask<Row?>` for direct
advancement. A cursor is single-pass, and overlapping session/cursor operations
are rejected. Sequential calls may resume on different threads.

`Rows.AffectedRows` is a nullable `long`: zero means a DML statement touched no
rows; `null` means the statement has no command count. Prepared `ExecuteAsync`
drains the cursor and returns the same nullable count. `CREATE TABLE AS` reports
the copied row count, except `WITH NO DATA` and skipped `IF NOT EXISTS` commands.

Native parameters use `SqlValue` implicit conversions, preserving the same static
inference and range checks as explicit values:

```csharp
await session.ExecuteAsync("INSERT INTO items (id, name) VALUES ($1, $2)", [42, "sample"]);
await foreach (var item in session.QueryAsync(
    "SELECT id, name FROM items WHERE id = $1",
    row => (Id: row.Get<int>(0), Name: row.Get<string>(1)), [42]))
    Console.WriteLine(item);
```

`Row.Get<T>` checks the SQL family and destination range; nullable/reference
destinations accept SQL NULL, while a nonnullable destination rejects it.
`Rows.GetOrdinal` resolves a column name once for indexed reads. `Row.Values()`
returns native scalars and engine container values; the raw `row[index]` remains
available for full fidelity. `SqlValue.From(object)` handles nullable scalars,
one-dimensional native arrays and `ISqlValueProvider` conversions. `SqlArray`
supplies explicit multidimensional shapes. Large SQL decimals remain
`ExactDecimal`; conversion to `System.Decimal` rejects precision loss.

`DateTimeOffset` parameters bind their instant to `timestamptz` and their own
calendar fields to `timestamp`/`date`, independently of the session zone, including
array elements. UTC `DateTime` carries an instant; unspecified `DateTime` carries
calendar fields only. Local `DateTime` requires explicit conversion to
`DateTimeOffset`. `DateOnly` binds a date. Sub-microsecond timestamp ticks are
truncated toward the earlier microsecond. Native reads distinguish unspecified
`DateTime` from UTC `DateTimeOffset`; values outside the CLR temporal range stay
raw `SqlValue` in `Values()`, and typed temporal getters reject them.

`QueryFirstOrDefaultAsync` returns the first owned row or null and closes the
cursor. The generic mapping overload streams through a caller-supplied delegate;
breaking iteration or a mapper exception closes the cursor. It uses no reflection
or implicit result collection and works under NativeAOT.

Outside inputs and host functions belong to an immutable `DatabaseEnvironment`:

```csharp
var environment = DatabaseEnvironment.Builder()
    .RegisterFunction(new HostFunction("host_identity", [SqlType.I64], SqlType.I64,
        arguments => arguments[0])
    {
        Volatility = FunctionVolatility.Immutable,
        Cost = 1,
        ComponentId = "example",
        SemanticVersion = 1
    })
    .Build();
await using var custom = await Database.CreateAsync(new() { Environment = environment });
```

The builder also loads pinned Unicode/time-zone bundles and accepts injected clock
and random sources. Builder changes do not alter previously built environments;
explicitly shared stateful sources remain shared. Temporary databases and attached
files inherit the environment. Persisted host-dependent expressions pin component
and semantic versions; an incompatible registration requires an explicit rebuild.
Host kernels are strict about NULL and their result types are checked. A batch
kernel appends results in row order; a thrown error identifies the first row after
that successful prefix. Host code remains outside the built-in safety guarantees.

Nonvolatile batch kernels with trivial arguments can prefetch up to 1,024 rows
already held by an eager buffer, resident sort or DISTINCT stage. Streaming
primary/full scans over a single interval, spilled sort output and grouped output
spools use windows of at most 64 rows. Cost headroom and the remaining LIMIT window
reduce those caps. Replay preserves ordinary per-row charges, NULL handling,
result checks and error order; read-ahead defers a source error until its preceding
rows have been consumed and never fetches a deferred large value speculatively.

Disjoint-interval and secondary-index streaming paths currently call batch
kernels one row at a time. A spilled sort with an active query-memory account also
uses one-row windows to preserve its charged merge-head schedule. Volatile,
zero-argument, conditional and nontrivial-argument calls stay scalar. These are
optimization limits: chunk sizes and callback counts are host-observable, but are
not a cross-core conformance contract ([batch ABI](../../spec/design/extensibility.md)).

`SqlException.SqlState` is the stable error identity. Cancellation throws
`SqlCanceledException`, an `OperationCanceledException` carrying SQLSTATE `57014`
through `ISqlError`; tasks and value tasks enter the canceled state. Cursor cost
and partial error cost remain available. Dispose an unread cursor to close it
without executing its expressions.

Resident scalar operations complete inline. File reads, recovery, commit and the
page-client transport await I/O through the same semantic core. Durable barriers
use the required OS synchronization operation. There is no Task.Run wrapper.
`Database.CreateAsync(new() { Path = "example.jed" })` creates a new file;
`Database.OpenAsync("example.jed")` opens an existing file. Omitting `Path` creates
an in-memory database through the same options surface. These expose the experimental
Linux file host; use separate sessions for independent cursors and transactions.
`DatabaseFileOptions.Synchronous` defaults to `Synchronous.On`. `Off` keeps commits atomic
while batching durability; the page owner maintains the cumulative manifest and durable floor.
`FlushIntervalMilliseconds` defaults to 1000 (zero disables the time trigger). Close, maintenance,
cap and deadline flushes make the pending batch durable. The environment's
`IClockSource.NowMicroseconds()` returns Unix microseconds and supplies both
durability deadlines and SQL clock functions.

`Database.ChangeCredentialAsync` rewraps an encrypted file's data key without
rewriting its data pages. The current file owner performs the operation; retained
readers keep their existing data key. `SerializeAsync` exports a committed
plaintext snapshot, and `DeserializeAsync` copies a plaintext image into an
independent in-memory database. These surfaces have host and byte-fixture checks;
the verification record identifies the tested source revisions.
`SerializeAsync(name)`, `CompactAsync(name)`, `ChangeCredentialAsync(credential, name)`
and `RekeyAsync(credential, name)` also address an attachment by its exact name.
Maintenance checks that target's read-only state and active readers/writer; it leaves
the main database's generation unchanged. Passing a null credential to `RekeyAsync`
decrypts the selected file. An encrypted scratch policy on the parent is inherited
by its attached file handles.

The public surface includes exact decimal, temporal and recursive values, parameter
binding and reusable prepared syntax. The scope inventory and verification record
record the completed baseline gates and explicit platform/optimization limits.
JIT/NativeAOT evidence and diagnostic measurements are tied to specific artifacts.
No public package compatibility promise is
made by these experimental APIs.
