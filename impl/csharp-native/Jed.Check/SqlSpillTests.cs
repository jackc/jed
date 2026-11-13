using Jed;

internal static class SqlSpillTests
{
    private sealed class ScratchCounter : IScratchDeviceHost
    {
        internal int Created;
        public ValueTask<IBlockStore> CreateAsync(string identity, string? directory, CancellationToken token)
        { Created++; return NativeScratchDeviceHost.Instance.CreateAsync(identity, directory, token); }
    }
    internal static async Task Run()
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-sql-spill-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            foreach (var encrypted in new[] { false, true })
            {
                var counter = new ScratchCounter();
                using var credential = encrypted ? DatabaseCredential.FromKey(new byte[32]) : null;
                await using var database = await Database.CreateFileAsync(Path.Combine(directory, encrypted ? "encrypted" : "plain"),
                    new() { Locking = FileLocking.None, Credential = credential, ScratchDevices = counter });
                Corpus.Equal(Database.DefaultWorkMem, database.WorkMem, "zero open option uses default work_mem");
                await using var session = database.CreateSession();
                await DatabaseFileTests.Exec(session, "CREATE TABLE t (id i32 PRIMARY KEY, g i32, pad text)");
                await DatabaseFileTests.Exec(session, "INSERT INTO t SELECT x, x % 7, repeat('p', 40 + x % 13) FROM generate_series(1, 100) x");
                foreach (var sql in new[] {
                    "SELECT id, g FROM t ORDER BY g, id DESC",
                    "SELECT id FROM t ORDER BY length(pad), id DESC LIMIT 21 OFFSET 3",
                    "SELECT id FROM t ORDER BY g LIMIT 15",
                    "SELECT id FROM t ORDER BY length(pad) LIMIT 0 OFFSET 8" })
                {
                    database.SetWorkMem(0);
                    var resident = await Collect(session, sql, 0);
                    database.SetWorkMem(128); var before = counter.Created;
                    var spilled = await Collect(session, sql, 1);
                    Corpus.Equal(resident.Values, spilled.Values, "SQL spill values and stable order");
                    Corpus.Equal(resident.Cost, spilled.Cost, "SQL spill cost invariance");
                    Corpus.Equal(true, counter.Created > before, "SQL operator used real scratch files");
                }
                database.SetWorkMem(128);
                await using (var rows = await session.QueryAsync("SELECT id FROM t ORDER BY pad, id", new QueryOptions { MaxQueryMemoryBytes = 1 }))
                    _ = await rows.ReadAsync();
                await DatabaseFileTests.Exec(session, "INSERT INTO t VALUES (101, 3, 'after early close')");
                Corpus.Equal("101", await DatabaseFileTests.Scalar(session, "SELECT count(*) FROM t"), "early spilled cursor releases snapshot");
            }
        }
        finally { Directory.Delete(directory, true); }
        Console.WriteLine("PASS SQL spill: real plaintext/encrypted external sorting, stable ties, expression keys, tiny account, cost invariance and early disposal");
    }
    private static async Task<(string Values, long Cost)> Collect(Session session, string sql, long budget)
    {
        await using var rows = await session.QueryAsync(sql, new QueryOptions { MaxQueryMemoryBytes = budget });
        var values = new List<string>();
        await foreach (var row in rows) values.Add(string.Join("|", Enumerable.Range(0, row.Count).Select(i => row[i].ToString())));
        return (string.Join("\n", values), rows.Cost);
    }
}
