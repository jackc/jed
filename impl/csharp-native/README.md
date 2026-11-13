# Native C# experiment

An independent, handwritten C# engine experiment targeting **.NET 11** and Linux
x86-64. It is opt-in and **incomplete**: the current slice executes scalar SELECT
without FROM. There is no database storage, transaction API, encryption or full
SQL parity yet. See the [charter](../../spec/design/csharp-native-experiment.md),
[scope](../../spec/design/csharp-native-scope.md) and
[verification record](VERIFICATION.md).

## Run explicitly

```sh
mise run csharp-native:setup
mise run csharp-native:codegen --check
mise run csharp-native:test
mise run csharp-native:test --runtime-async
mise run csharp-native:aot --runtime-async
mise run csharp-native:inventory
```

Setup downloads and SHA-512-verifies Microsoft SDK 11.0.100 into this checkout's
ignored `.dev/csharp-native/` directory. `--archive /absolute/path/to/sdk.tar.gz`
uses an existing download with the same checksum check. Ordinary build/test has
no NuGet dependencies and no package feeds. The explicit AOT task restores the
SDK's Microsoft compiler/runtime packs and needs a native compiler such as clang.
It does not add engine dependencies.

`csharp-native:conformance --file path/to/file.test` attempts one complete file.
`--all` attempts the pinned baseline and is expected to fail while the engine and
adapters are incomplete. Neither option silently skips unsupported records.
The default conformance selection is the shared scalar foundation supplement.
No normal repository task builds or installs this experiment.

## Async embedding

```csharp
using Jed;

await using var session = new Session(); // Scalar-only until storage is implemented.
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

`SqlException.SqlState` is the stable error identity. Cancellation throws
`SqlCanceledException`, an `OperationCanceledException` carrying SQLSTATE `57014`
through `ISqlError`; tasks and value tasks enter the canceled state. Cursor cost
and partial error cost remain available. Dispose an unread cursor to close it
without executing its expressions.

Resident scalar operations complete inline. This slice performs no I/O and does
not claim asynchronous storage support. Future page devices, recovery and commit
will await actual I/O through the same semantic core. There is no Task.Run
wrapper or synchronous wait on an asynchronous operation. Runtime Async and
NativeAOT are verified configurations for this scalar slice; their performance is unmeasured.

Only integer, Boolean, text and NULL representations are present. Complete type
support, parameters/prepared statements, other expression forms, resource memory
accounts, table storage and host functions remain in the scope inventory. No
public package compatibility promise is made by these experimental APIs.
