using System.Buffers.Binary;
using Jed;

internal static class DatabaseBackupTests
{
    internal static async Task Run(string root)
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-backup-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        var fixtures = Path.Combine(root, "spec/fileformat/fixtures");
        var names = new[] { "empty_db", "tall_tree", "overflow_table", "compressed_table", "index_table", "gin_multi_table", "gist_range_table" }
            .Concat(Directory.GetFiles(fixtures, "encrypted_*.jed").Order(StringComparer.Ordinal).Select(Path.GetFileNameWithoutExtension)).ToArray();
        var host = CryptoTests.ReadVectors(Path.Combine(root, "spec/fileformat/encryption_vectors.toml")).Single(v => v.Name == "host_kek");
        using var key = DatabaseCredential.FromKey(CryptoTests.Hex(host, "key_hex"));
        try
        {
            foreach (var name in names)
            {
                var original = await File.ReadAllBytesAsync(Path.Combine(fixtures, name + ".jed"));
                var encrypted = original[6] != 0; var size = (int)BinaryPrimitives.ReadUInt32BigEndian(original.AsSpan(8));
                var path = Path.Combine(directory, name + ".jed"); await File.WriteAllBytesAsync(path, original);
                var options = new DatabaseFileOptions { Credential = encrypted ? key : null };
                await using var database = await Database.OpenFileAsync(path, options);
                foreach (var compacted in new[] { false, true })
                {
                    var destination = path + (compacted ? ".compact" : ".identical");
                    await database.BackupForTestingAsync(destination, compacted);
                    var copied = await File.ReadAllBytesAsync(destination);
                    if (!compacted || !encrypted)
                        Corpus.Equal(true, original.AsSpan(2 * size).SequenceEqual(copied.AsSpan(2 * size)), "backup exact stored pages " + name);
                    if (!encrypted && !compacted) Corpus.Equal(true, original.AsSpan().SequenceEqual(copied), "plaintext backup complete golden " + name);
                    await using var imported = await Database.OpenFileAsync(destination, options);
                    await using var pin = await imported.PinAsync();
                    Corpus.Equal(BinaryPrimitives.ReadUInt64BigEndian(original.AsSpan(12)) + (compacted ? 1ul : 0ul), pin.Snapshot.Meta.TransactionId, "backup transaction identifier");
                    foreach (var table in pin.Snapshot.Catalog.Tables)
                    {
                        long count = 0; var tree = imported.Tree(table, null, pin.Snapshot);
                        await foreach (var row in tree.ScanAsync()) { _ = await tree.MaterializeRecordAsync(row); count++; }
                        Corpus.Equal(table.RowCount, count, "backup complete authenticated tree/overflow walk");
                    }
                    await HostTests.Error(SqlStates.DuplicateFile, () => database.BackupForTestingAsync(destination, compacted));
                }
            }
            await using var memory = await Database.CreateInMemoryAsync();
            await HostTests.Error(SqlStates.FeatureNotSupported, () => memory.BackupForTestingAsync(Path.Combine(directory, "memory-identical"), false));
            await memory.BackupForTestingAsync(Path.Combine(directory, "memory-compact"), true);
            var busyDestination = Path.Combine(directory, "already-staging");
            await File.WriteAllTextAsync(busyDestination + ".jedtmp", "belongs to another operation");
            await HostTests.Error(SqlStates.IoError, () => memory.BackupForTestingAsync(busyDestination, true));
            Corpus.Equal("belongs to another operation", await File.ReadAllTextAsync(busyDestination + ".jedtmp"), "backup preserves another operation's staging file");
            foreach (var stage in new[] { "backup-before-rename", "backup-after-rename" })
            {
                var destination = Path.Combine(directory, stage);
                memory.MaintenanceStepForTesting = current => { if (current == stage) throw new IOException("injected backup publication failure"); };
                await HostTests.Error(SqlStates.IoError, () => memory.BackupForTestingAsync(destination, true));
                Corpus.Equal(false, File.Exists(destination), "failed backup removes destination");
                Corpus.Equal(false, File.Exists(destination + ".jedtmp"), "failed backup removes temp file");
                memory.MaintenanceStepForTesting = null;
            }
        }
        finally { Directory.Delete(directory, recursive: true); }
        Console.WriteLine($"PASS backup tooling: page-identical and compacted copies of {names.Length} shared plaintext/encrypted files, stored-byte identity, current keys, complete authenticated tree/overflow walks and destination refusal");
    }
}
