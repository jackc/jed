using Jed;

internal static class RekeyTests
{
    internal static async Task Run(string root)
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-rekey-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        try
        {
            using var credential = DatabaseCredential.FromKey(Enumerable.Repeat((byte)0x4a, 32).ToArray());
            using var replacement = DatabaseCredential.FromKey(Enumerable.Repeat((byte)0x5b, 32).ToArray());
            var path = Path.Combine(directory, "golden.jed");
            var entropy = new SeededRandomSource(1246053442);
            var options = new DatabaseFileOptions { ScratchEncryption = ScratchEncryption.On, CacheBytes = 1024, KeyEntropy = entropy.Fill };
            await using (var database = await Database.CreateFileAsync(path, options, 1024))
            {
                await using var session = database.CreateSession();
                await DatabaseFileTests.Exec(session, "CREATE TABLE t(a i32 PRIMARY KEY, v i64, r i32range)");
                await DatabaseFileTests.Exec(session, "CREATE INDEX t_r ON t USING gist(r)");
                await DatabaseFileTests.Exec(session, "CREATE INDEX t_v ON t(v)");
                for (var i = 150; i >= 1; i--) await DatabaseFileTests.Exec(session, $"INSERT INTO t VALUES({i},{i * 3},i32range({i},{i + 5}))");
                Corpus.Equal(154UL, database.Version, "rekey source generation");
                await using (var reader = await database.OpenReadSessionAsync())
                    await HostTests.Error(SqlStates.ObjectInUse, () => database.RekeyAsync(credential));
                var before = await File.ReadAllBytesAsync(path);
                database.MaxStorageBytes = 1;
                await HostTests.Error(SqlStates.StorageLimitExceeded, () => database.RekeyAsync(credential));
                var rejected = await File.ReadAllBytesAsync(path);
                Corpus.Equal(true, before.AsSpan().SequenceEqual(rejected), "rekey storage limit leaves source bytes unchanged");
                database.MaxStorageBytes = 0; entropy.Reseed(1246053442);
                await database.RekeyAsync(credential);
                var expected = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/encrypted_rekey_index_table.jed"));
                var image = await File.ReadAllBytesAsync(path);
                if (!expected.AsSpan().SequenceEqual(image))
                {
                    await File.WriteAllBytesAsync("/tmp/jed-csharp-rekey-actual.jed", image);
                    throw new InvalidDataException($"rekey image differs: expected {expected.Length}, actual {image.Length}; first byte {Enumerable.Range(0, Math.Min(expected.Length, image.Length)).FirstOrDefault(i => expected[i] != image[i], -1)}");
                }
                Corpus.Equal("150", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "encrypted target remains usable");
                await database.RekeyAsync(null);
                Corpus.Equal("150", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t WHERE r && i32range(0,200)"), "decrypted GiST target remains usable");
                var plaintext = await File.ReadAllBytesAsync(path);
                database.MaintenanceStepForTesting = stage => { if (stage == "before-rename") throw new IOException("injected rename refusal"); };
                await HostTests.Error(SqlStates.IoError, () => database.RekeyAsync(replacement));
                database.MaintenanceStepForTesting = null;
                var unchanged = await File.ReadAllBytesAsync(path);
                Corpus.Equal(true, plaintext.AsSpan().SequenceEqual(unchanged), "failed rekey rename preserves old inode bytes");
                Corpus.Equal("150", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "failed rekey rename keeps handle usable");
                await database.RekeyAsync(replacement);
            }
            await using (var reopened = await Database.OpenFileAsync(path, new() { Credential = replacement, CacheBytes = 0 }))
            {
                await using var session = reopened.CreateSession();
                Corpus.Equal("450", await DatabaseFileTests.Scalar(session, "SELECT v FROM t WHERE a=150"), "reopened rotated row");
            }
            await HostTests.Error(SqlStates.InvalidPassword, async () => { await using var invalid = await Database.OpenFileAsync(path, new() { Credential = credential }); });
            var plain = Path.Combine(directory, "insecure-scratch.jed");
            await using (var database = await Database.CreateFileAsync(plain))
                await HostTests.Error(SqlStates.ObjectNotInPrerequisiteState, () => database.RekeyAsync(credential));
            await using (var memory = await Database.CreateInMemoryAsync())
                await HostTests.Error(SqlStates.ObjectNotInPrerequisiteState, () => memory.RekeyAsync(credential));
        }
        finally { Directory.Delete(directory, recursive: true); }
        Console.WriteLine("PASS rekey: complete encrypted golden, ascending table/B-tree/GiST rebuild with spilling scratch, rotation/decryption, storage limit, nonwaiting pins and rename-failure recovery");
    }
}
