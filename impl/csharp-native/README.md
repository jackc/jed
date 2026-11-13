# Native C# experiment

An independent, handwritten C# engine experiment targeting **.NET 11** and Linux
x86-64. It is opt-in and **incomplete**. Native SQL execution, paged storage,
transactions, encrypted files and owner/client process coordination are implemented
and undergoing shared-corpus verification. Full baseline parity is still pending. See the [charter](../../spec/design/csharp-native-experiment.md),
[scope](../../spec/design/csharp-native-scope.md) and
[verification record](VERIFICATION.md).

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
`--suite name` attempts a pinned suite; `--all` attempts the pinned baseline and is expected to fail while the engine and
adapters are incomplete. Neither option silently skips unsupported records.
The default conformance selection is the shared scalar foundation supplement.
No normal repository task builds or installs this experiment.

## Async embedding

```csharp
using Jed;

await using var database = await Database.CreateInMemoryAsync();
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

`SqlException.SqlState` is the stable error identity. Cancellation throws
`SqlCanceledException`, an `OperationCanceledException` carrying SQLSTATE `57014`
through `ISqlError`; tasks and value tasks enter the canceled state. Cursor cost
and partial error cost remain available. Dispose an unread cursor to close it
without executing its expressions.

Resident scalar operations complete inline. File reads, recovery, commit and the
page-client transport await I/O through the same semantic core. Durable barriers
use the required OS synchronization operation. There is no Task.Run wrapper.
`Database.CreateFileAsync` and `Database.OpenFileAsync` expose the experimental
Linux file host; use separate sessions for independent cursors and transactions.

The public surface includes exact decimal, temporal and recursive values, parameter
binding and reusable prepared syntax. The scope inventory records missing SQL,
resource-accounting, spill, host and maintenance behavior. JIT/NativeAOT evidence
is tied to specific source digests in the verification record; performance remains
unmeasured. No public package compatibility promise is made by these experimental
APIs.
