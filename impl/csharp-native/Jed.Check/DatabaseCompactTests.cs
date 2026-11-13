using System.Buffers.Binary;
using Jed;

internal static class DatabaseCompactTests
{
    internal static async Task Run(string root)
    {
        var fixture = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/overflow_table.jed"));
        await using (var database = await Database.DeserializeAsync(fixture))
        {
            await database.CompactAsync();
            var after = await database.SerializeAsync();
            var size = (int)BinaryPrimitives.ReadUInt32BigEndian(fixture.AsSpan(8));
            Corpus.Equal(true, fixture.AsSpan(2 * size).SequenceEqual(after.AsSpan(2 * size)), "memory compaction preserves node shape, compression and overflow dispositions");
            Corpus.Equal(BinaryPrimitives.ReadUInt64BigEndian(fixture.AsSpan(12)) + 1,
                BinaryPrimitives.ReadUInt64BigEndian(after.AsSpan(12)), "memory compaction advances version");
        }
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-compact-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        try
        {
            foreach (var encrypted in new[] { false, true })
            {
                var path = Path.Combine(directory, encrypted ? "encrypted.jed" : "plain.jed");
                using var credential = encrypted ? DatabaseCredential.FromKey(new byte[32]) : null;
                var options = new DatabaseFileOptions { Credential = credential }; var size = encrypted ? 1024 : 256;
                await using (var database = await Database.CreateFileAsync(path, options, size))
                {
                    await using var session = database.CreateSession();
                    await DatabaseFileTests.Exec(session, "CREATE TABLE t(id i32 PRIMARY KEY, value text)");
                    await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (1,'first'),(2,'second')");
                    await DatabaseFileTests.Exec(session, "UPDATE t SET value='changed' WHERE id=2");
                }
                var original = await File.ReadAllBytesAsync(path);
                var enlarged = new byte[original.Length + size * 5]; original.CopyTo(enlarged, 0); await File.WriteAllBytesAsync(path, enlarged);
                await using (var database = await Database.OpenFileAsync(path, options))
                {
                    await using var session = database.CreateSession();
                    await using (var cursor = await session.QueryAsync("SELECT id FROM t"))
                        await HostTests.Error(SqlStates.ObjectInUse, () => database.CompactAsync());
                    await DatabaseFileTests.Exec(session, "BEGIN");
                    await HostTests.Error(SqlStates.ObjectInUse, () => database.CompactAsync());
                    await DatabaseFileTests.Exec(session, "ROLLBACK");
                    var backupPath = path + ".backup";
                    await database.BackupForTestingAsync(backupPath, compacted: false);
                    await using (var backup = await Database.OpenFileAsync(backupPath, options))
                    {
                        await using var read = backup.CreateSession();
                        Corpus.Equal("2", await DatabaseFileTests.Scalar(read, "SELECT count(*) FROM t"), "incremental backup ignores manifest and preserves free-list accounting");
                    }
                    await HostTests.Error(SqlStates.UndefinedObject, () => database.CompactAsync("missing"));
                    await database.CompactAsync();
                    var after = await File.ReadAllBytesAsync(path);
                    Corpus.Equal(true, after.Length < enlarged.Length, "compaction removes orphan/free/tail pages");
                    var meta = encrypted ? StorageCryptoMeta.Parse(after.AsSpan(0, size), after.Length)!.Header : StorageFormat.ParseMeta(after.AsSpan(0, size), after.Length)!;
                    Corpus.Equal((long)after.Length, (long)meta.PageCount * size, "compaction exact file length");
                    Corpus.Equal(0u, meta.FreeListHead, "compaction has no free list");
                    Corpus.Equal(meta.PageCount - (encrypted ? 4u : 2u), meta.LivePages, "compaction exact live pages");
                    if (encrypted) Corpus.Equal(true, original.AsSpan(2 * size, 2 * size).SequenceEqual(after.AsSpan(2 * size, 2 * size)), "encrypted compaction retains exact key blocks");
                    Corpus.Equal("changed", await DatabaseFileTests.Scalar(session, "SELECT value FROM t WHERE id=2"), "compaction retains SQL values");
                    await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (3,'after')");
                }
                await using (var readOnly = await Database.OpenFileAsync(path, options with { ReadOnly = true }))
                {
                    await HostTests.Error(SqlStates.ReadOnlySqlTransaction, () => readOnly.CompactAsync());
                    await using var session = readOnly.CreateSession();
                    Corpus.Equal("3", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "compacted file and later commit reopen");
                }
            }
            await Failures(directory);
        }
        finally { Directory.Delete(directory, recursive: true); }
        Console.WriteLine("PASS compaction: node-preserving memory images, plaintext/encrypted file replacement, key preservation, exact live size, retained data, reader/writer rejection and durable reopen");
    }

    private static async Task Failures(string directory)
    {
        foreach (var encrypted in new[] { false, true })
            foreach (var stage in new[] { "image-synced", "before-rename", "after-rename", "before-publish" })
            {
                var path = Path.Combine(directory, $"fault-{encrypted}-{stage}.jed");
                using var key = encrypted ? DatabaseCredential.FromKey(new byte[32]) : null;
                var options = new DatabaseFileOptions { Credential = key };
                await using (var database = await Database.CreateFileAsync(path, options, 1024))
                {
                    await using var session = database.CreateSession();
                    await DatabaseFileTests.Exec(session, "CREATE TABLE t(id i32 PRIMARY KEY)");
                    await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (7)");
                    var original = await File.ReadAllBytesAsync(path);
                    database.MaintenanceStepForTesting = current => { if (current == stage) throw new IOException("injected maintenance boundary failure"); };
                    await HostTests.Error(SqlStates.IoError, () => database.CompactAsync());
                    database.MaintenanceStepForTesting = null;
                    Corpus.Equal(false, File.Exists(path + ".jedtmp"), "failed compaction cleans reserved temp path");
                    if (stage is "image-synced" or "before-rename")
                    {
                        var unchanged = await File.ReadAllBytesAsync(path);
                        Corpus.Equal(true, original.AsSpan().SequenceEqual(unchanged), "pre-rename failure preserves source bytes");
                        Corpus.Equal("7", await DatabaseFileTests.Scalar(session, "SELECT id FROM t"), "pre-rename failure preserves usable handle");
                    }
                    else await HostTests.Error(SqlStates.IoError, async () => { _ = await DatabaseFileTests.Scalar(session, "SELECT id FROM t"); });
                }
                await using var reopened = await Database.OpenFileAsync(path, options);
                await using var read = reopened.CreateSession();
                Corpus.Equal("7", await DatabaseFileTests.Scalar(read, "SELECT id FROM t"), "every failed compaction leaves a complete reopenable image");
            }
    }
}
