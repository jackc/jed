using System.Security.Cryptography;
using Jed;

internal static class CredentialChangeTests
{
    internal static async Task Run(string root)
    {
        await Barriers(root);
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-credential-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        var path = Path.Combine(directory, "main.jed");
        using var old = DatabaseCredential.FromKey(new byte[32]);
        using var replacement = DatabaseCredential.FromPassphrase("replacement credential"u8, 8, 1, 1);
        try
        {
            await NamedMaintenance(directory);
            await RepairInterruptedChange(root, directory);
            await using (var memory = await Database.CreateInMemoryAsync())
                await HostTests.Error(SqlStates.ObjectNotInPrerequisiteState, () => memory.ChangeCredentialAsync(replacement));
            await using (var database = await Database.CreateFileAsync(path, new() { Credential = old }, 1024))
            {
                await using var session = database.CreateSession();
                await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i32 PRIMARY KEY)");
                await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (7)");
                await HostTests.Error(SqlStates.UndefinedObject, () => database.ChangeCredentialAsync(replacement, "missing"));
                await DatabaseFileTests.Exec(session, "BEGIN");
                await HostTests.Error(SqlStates.ObjectInUse, () => database.ChangeCredentialAsync(replacement));
                await DatabaseFileTests.Exec(session, "ROLLBACK");
                var before = await File.ReadAllBytesAsync(path);
                await using var frozen = await session.QueryAsync("SELECT id FROM t");
                await database.ChangeCredentialAsync(replacement);
                var after = await File.ReadAllBytesAsync(path);
                Corpus.Equal(before.Length, after.Length, "rewrap preserves file length");
                Corpus.Equal(true, before.AsSpan(0, 2048).SequenceEqual(after.AsSpan(0, 2048)), "rewrap preserves both meta pages");
                Corpus.Equal(true, before.AsSpan(4096).SequenceEqual(after.AsSpan(4096)), "rewrap preserves every data/manifest/free page");
                Corpus.Equal("7", (await frozen.ReadAsync())![0].ToString(), "reader survives key rewrap");
                await frozen.DisposeAsync();
                Corpus.Equal("7", await DatabaseFileTests.Scalar(session, "SELECT id FROM t"), "handle retains its data key after rewrap");
            }
            await HostTests.Error(SqlStates.InvalidPassword, async () => { await using var failed = await Database.OpenFileAsync(path, new() { Credential = old }); });
            await using (var database = await Database.OpenFileAsync(path, new() { Credential = replacement, ReadOnly = true }))
            {
                await HostTests.Error(SqlStates.ReadOnlySqlTransaction, () => database.ChangeCredentialAsync(old));
                await using var session = database.CreateSession();
                Corpus.Equal("7", await DatabaseFileTests.Scalar(session, "SELECT id FROM t"), "new passphrase opens existing encrypted pages");
            }
            var plain = Path.Combine(directory, "plain.jed");
            await using (var database = await Database.CreateFileAsync(plain))
                await HostTests.Error(SqlStates.CredentialNotApplicable, () => database.ChangeCredentialAsync(replacement));
        }
        finally { Directory.Delete(directory, recursive: true); }
        Console.WriteLine("PASS credential maintenance: key-block-only rewrap, host-key→passphrase reopen, retained reader/key, nonwaiting writer gate, errors and fault injection at all five storage stages");
    }

    private static async Task NamedMaintenance(string directory)
    {
        var path = Path.Combine(directory, "attached.jed");
        using var old = DatabaseCredential.FromKey(new byte[32]);
        using var next = DatabaseCredential.FromKey(Enumerable.Repeat((byte)41, 32).ToArray());
        await using (var attached = await Database.CreateFileAsync(path, new() { Credential = old, Locking = FileLocking.None }, 1024))
        {
            await using var session = attached.CreateSession();
            await DatabaseFileTests.Exec(session, "CREATE TABLE t(id i32 PRIMARY KEY)");
            await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES(9)");
        }
        await using var main = await Database.CreateFileAsync(Path.Combine(directory, "named-main.jed"),
            new() { Locking = FileLocking.None, ScratchEncryption = ScratchEncryption.On });
        await main.AttachFileAsync("child", path, new() { Credential = old, Locking = FileLocking.None });
        var mainVersion = main.Version;
        await main.ChangeCredentialAsync(next, "child");
        await main.CompactAsync("child");
        await main.RekeyAsync(null, "child");
        var plaintext = await main.SerializeAsync("child");
        await using var exported = await Database.DeserializeAsync(plaintext);
        await using var read = exported.CreateSession();
        Corpus.Equal("9", await DatabaseFileTests.Scalar(read, "SELECT id FROM t"), "named rewrap/compact/decrypt/export retain attachment contents");
        await main.RekeyAsync(next, "child");
        Corpus.Equal(mainVersion, main.Version, "named cryptographic maintenance leaves main unchanged");
        await main.DetachAsync("child");
        await HostTests.Error(SqlStates.InvalidPassword, async () => { await using var invalid = await Database.OpenFileAsync(path, new() { Credential = old, Locking = FileLocking.None }); });
        await using var reopened = await Database.OpenFileAsync(path, new() { Credential = next, Locking = FileLocking.None });
        await using var reopenedSession = reopened.CreateSession();
        Corpus.Equal("9", await DatabaseFileTests.Scalar(reopenedSession, "SELECT id FROM t"), "named rekey persists independent file credential");
    }

    private static async Task RepairInterruptedChange(string root, string directory)
    {
        var image = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/encrypted_empty_db.jed"));
        var host = CryptoTests.ReadVectors(Path.Combine(root, "spec/fileformat/encryption_vectors.toml")).Single(v => v.Name == "host_kek");
        using var original = StorageCredential.HostKey(CryptoTests.Hex(host, "key_hex"));
        using var changed = DatabaseCredential.FromKey(Enumerable.Repeat((byte)57, 32).ToArray());
        var selected = StorageKeyBlock.Select(image.AsSpan(2048, 1024), image.AsSpan(3072, 1024));
        var dek = selected.Block.Unwrap(original);
        try
        {
            var replacement = StorageKeyBlock.Create(1024, selected.Block.Generation + 1, dek, changed.Value, count => new byte[count]);
            replacement.Bytes.Span.CopyTo(image.AsSpan(3072));
            var path = Path.Combine(directory, "interrupted.jed");
            await File.WriteAllBytesAsync(path, image);
            await using (var readOnly = await Database.OpenFileAsync(path, new() { Credential = changed, ReadOnly = true }))
            {
                var unchanged = await File.ReadAllBytesAsync(path);
                Corpus.Equal(true, image.AsSpan().SequenceEqual(unchanged), "read-only open preserves divergent key copies");
            }
            await using (var writable = await Database.OpenFileAsync(path, new() { Credential = changed }))
            {
                var repaired = await File.ReadAllBytesAsync(path);
                Corpus.Equal(true, repaired.AsSpan(2048, 1024).SequenceEqual(repaired.AsSpan(3072, 1024)), "writable open repairs interrupted key change");
                var selection = StorageKeyBlock.Select(repaired.AsSpan(2048, 1024), repaired.AsSpan(3072, 1024));
                Corpus.Equal(2u, selection.Page, "repaired equal copies select page two");
                Corpus.Equal(replacement.Generation, selection.Block.Generation, "repair retains newest key generation");
            }
        }
        finally { CryptographicOperations.ZeroMemory(dek); }
    }

    private static async Task Barriers(string root)
    {
        var image = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/encrypted_empty_db.jed"));
        var host = CryptoTests.ReadVectors(Path.Combine(root, "spec/fileformat/encryption_vectors.toml")).Single(v => v.Name == "host_kek");
        using var original = StorageCredential.HostKey(CryptoTests.Hex(host, "key_hex"));
        using var replacement = StorageCredential.HostKey(Enumerable.Repeat((byte)57, 32).ToArray());
        const int size = 1024;
        var selected = StorageKeyBlock.Select(image.AsSpan(2 * size, size), image.AsSpan(3 * size, size));
        var dek = selected.Block.Unwrap(original);
        try
        {
            var wrapped = StorageKeyBlock.Create(size, selected.Block.Generation + 1, dek, replacement, count => new byte[count]);
            for (var step = 0; step < 5; step++)
                foreach (var after in new[] { false, true })
                {
                    await using var fault = new RewrapFaultStore(image, step, after);
                    try { _ = await StorageCredentialChange.ReplaceAsync(fault, selected, wrapped); throw new InvalidDataException("rewrap fault was not reached"); }
                    catch (IOException) { }
                    var durable = StorageKeyBlock.Select(fault.Durable.AsSpan(2 * size, size), fault.Durable.AsSpan(3 * size, size));
                    var newDurable = step > 2 || step == 2 && after;
                    Corpus.Equal(newDurable ? wrapped.Generation : selected.Block.Generation, durable.Block.Generation, "rewrap crash selects durable generation");
                    var key = durable.Block.Unwrap(newDurable ? replacement : original);
                    try { Corpus.Equal(true, key.AsSpan().SequenceEqual(dek), "rewrap never rotates DEK"); }
                    finally { CryptographicOperations.ZeroMemory(key); }
                    try { _ = durable.Block.Unwrap(newDurable ? original : replacement); throw new InvalidDataException("both credentials unlock selected key"); }
                    catch (SqlException error) when (error.SqlState == SqlStates.InvalidPassword) { }
                }
        }
        finally { CryptographicOperations.ZeroMemory(dek); }
    }

    private sealed class RewrapFaultStore(byte[] initial, int failedStep, bool failAfter) : IBlockStore
    {
        private readonly byte[] current = initial.ToArray();
        internal byte[] Durable { get; private set; } = initial.ToArray();
        private int step;
        private void Mutate(Action action)
        {
            var fail = step++ == failedStep;
            if (fail && !failAfter) throw new IOException("injected pre-stage failure");
            action();
            if (fail) throw new IOException("injected post-stage failure");
        }
        public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
        { current.AsMemory((int)offset, destination.Length).CopyTo(destination); return ValueTask.CompletedTask; }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> bytes, CancellationToken cancellationToken = default)
        { Mutate(() => bytes.CopyTo(current.AsMemory((int)offset))); return ValueTask.CompletedTask; }
        public ValueTask SyncAsync(CancellationToken cancellationToken = default)
        { Mutate(() => Durable = current.ToArray()); return ValueTask.CompletedTask; }
        public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default) => new(current.Length);
        public ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default) => throw new InvalidOperationException("rewrap must not resize");
        public ValueTask DisposeAsync() => ValueTask.CompletedTask;
    }
}
