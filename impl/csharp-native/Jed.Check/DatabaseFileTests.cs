using Jed;

internal static class DatabaseFileTests
{
    internal static async Task Run()
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-csharp-file-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        var path = Path.Combine(directory, "main.jed");
        try
        {
            await using (var database = await Database.CreateFileAsync(path, pageSize: 256))
            {
                await using var session = database.CreateSession();
                await Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY, n i64)");
                await Exec(session, "INSERT INTO t VALUES (1,11),(2,22)");
                Corpus.Equal("22", await Scalar(session, "SELECT n FROM t WHERE id=2"), "file owner reads own commit");
                await HostTests.Error(SqlStates.ObjectInUse, async () => { await using var duplicate = await Database.OpenFileAsync(path); });
                await HostTests.Error(SqlStates.ObjectInUse, async () => { await using var duplicate = await Database.CreateFileAsync(path); });
            }
            Corpus.Equal(true, Directory.Exists(path + ".lock"), "database close preserves bundle");
            Corpus.Equal(false, File.Exists(Path.Combine(path + ".lock", "owner")), "database close removes owner record");
            await using (var reopened = await Database.OpenFileAsync(path))
            {
                await using var session = reopened.CreateSession();
                Corpus.Equal("11", await Scalar(session, "SELECT n FROM t WHERE id=1"), "durable reopen");
                await Exec(session, "UPDATE t SET n=33 WHERE id=1");
            }
            await using (var readOnly = await Database.OpenFileAsync(path, new() { ReadOnly = true }))
            {
                await using var session = readOnly.CreateSession();
                Corpus.Equal("33", await Scalar(session, "SELECT n FROM t WHERE id=1"), "read-only reopen");
                await HostTests.Error(SqlStates.ReadOnlySqlTransaction, () => Exec(session, "DELETE FROM t"));
            }
            await HostTests.Error(SqlStates.DuplicateFile, async () => { await using var duplicate = await Database.CreateFileAsync(path); });
            await Encrypted(Path.Combine(directory, "encrypted.jed"), path);
            Console.WriteLine("PASS file database host: coordinated plaintext/encrypted create/open, owner SQL commits, durable reopen, credential errors, exact key-entropy order, duplicate claims, read-only writes and stable bundle lifetime");
        }
        finally { Directory.Delete(directory, recursive: true); }
    }
    private static async Task Encrypted(string path, string plaintext)
    {
        using var credential = DatabaseCredential.FromKey(new byte[32]);
        using var wrong = DatabaseCredential.FromKey(Enumerable.Repeat((byte)1, 32).ToArray());
        var draws = new List<int>();
        byte[] Entropy(int length) { draws.Add(length); return Enumerable.Repeat((byte)draws.Count, length).ToArray(); }
        await HostTests.Error(SqlStates.FeatureNotSupported, async () => { await using var invalid = await Database.CreateFileAsync(path, new() { Credential = credential }, 256); });
        await using (var database = await Database.CreateFileAsync(path, new() { Credential = credential, KeyEntropy = Entropy }, 1024))
        {
            Corpus.Equal("32,32,12,24,24,24", string.Join(',', draws), "encrypted create entropy order excludes file coordination");
            await using var session = database.CreateSession();
            await Exec(session, "CREATE TABLE t (id i64 PRIMARY KEY, n i64)");
            await Exec(session, "INSERT INTO t VALUES (1,71)");
            Corpus.Equal("71", await Scalar(session, "SELECT n FROM t"), "encrypted file owner SQL");
        }
        await HostTests.Error(SqlStates.CredentialRequired, async () => { await using var missing = await Database.OpenFileAsync(path); });
        await HostTests.Error(SqlStates.InvalidPassword, async () => { await using var invalid = await Database.OpenFileAsync(path, new() { Credential = wrong }); });
        await HostTests.Error(SqlStates.CredentialNotApplicable, async () => { await using var invalid = await Database.OpenFileAsync(plaintext, new() { Credential = credential }); });
        await using (var reopened = await Database.OpenFileAsync(path, new() { Credential = credential }))
        {
            await using var session = reopened.CreateSession();
            Corpus.Equal("71", await Scalar(session, "SELECT n FROM t"), "encrypted durable reopen");
        }
    }
    internal static async Task Exec(Session session, string sql)
    { await using var rows = await session.QueryAsync(sql); await foreach (var _ in rows) { } }
    internal static async Task<string?> Scalar(Session session, string sql)
    {
        await using var rows = await session.QueryAsync(sql); var row = await rows.ReadAsync(); return row is null ? null : row[0].ToString();
    }
}
