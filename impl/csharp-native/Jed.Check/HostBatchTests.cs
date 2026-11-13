using Jed;

internal static class HostBatchTests
{
    private sealed record Outcome(string Values, string? Error, long Cost);
    internal static async Task Run()
    {
        var chunks = new List<int>();
        SqlValue Scalar(IReadOnlyList<SqlValue> values)
        {
            var value = values[0].AsInteger();
            if (value == 117) throw new SqlException("22012", "host row failure");
            return SqlValue.Integer(value * 2);
        }
        void Kernel(IReadOnlyList<IReadOnlyList<SqlValue>> columns, IList<SqlValue> output)
        {
            chunks.Add(columns[0].Count);
            foreach (var value in columns[0]) output.Add(Scalar([value]));
        }
        var batch = HostFunction.Batch("batch_probe", [SqlType.I64], SqlType.I64, Kernel, FunctionVolatility.Immutable, 3);
        var batchEnvironment = DatabaseEnvironment.Builder().RegisterFunction(batch).Build();
        var scalarEnvironment = DatabaseEnvironment.Builder().RegisterFunction(new("batch_probe", [SqlType.I64], SqlType.I64, Scalar)
        { Volatility = FunctionVolatility.Immutable, Cost = 3 }).Build();
        await using var batched = await Database.CreateInMemoryAsync(environment: batchEnvironment);
        await using var scalar = await Database.CreateInMemoryAsync(environment: scalarEnvironment);
        await using var batchSession = batched.CreateSession();
        await using var scalarSession = scalar.CreateSession();
        foreach (var session in new[] { batchSession, scalarSession })
        {
            await session.ExecuteAsync("CREATE TABLE batch_rows (id i64 PRIMARY KEY, v i64)");
            await session.ExecuteAsync("INSERT INTO batch_rows SELECT g, CASE WHEN g % 19 = 0 THEN NULL ELSE g END FROM generate_series(1, 240) g");
        }
        foreach (var sql in new[]
        {
            "SELECT batch_probe(v) FROM batch_rows WHERE id < 100 ORDER BY id % 7, id",
            "SELECT batch_probe(v) FROM batch_rows ORDER BY id % 7, id",
            "SELECT 1 / (id - 20), batch_probe(v) FROM batch_rows ORDER BY id % 7, id",
            "SELECT batch_probe(v), 1 / (id - 20) FROM batch_rows ORDER BY id % 7, id",
            "SELECT CASE WHEN id < 100 THEN batch_probe(v) ELSE 0 END FROM batch_rows ORDER BY id % 7, id",
            "SELECT batch_probe(v + 0) FROM batch_rows ORDER BY id % 7, id",
            "SELECT batch_probe(v) FROM batch_rows ORDER BY id % 7, id LIMIT 9 OFFSET 3",
            "SELECT batch_probe(v) FROM batch_rows ORDER BY id",
            "SELECT batch_probe(v) FROM batch_rows WHERE id BETWEEN 20 AND 80 ORDER BY id",
            "SELECT id FROM batch_rows WHERE batch_probe(v) > 100 ORDER BY id",
            "SELECT batch_probe(v) FROM batch_rows ORDER BY id LIMIT 9 OFFSET 3",
            "SELECT DISTINCT batch_probe(id) FROM batch_rows WHERE id < 100"
        })
        {
            chunks.Clear();
            var actual = await Read(batchSession, sql);
            var expected = await Read(scalarSession, sql);
            Corpus.Equal(expected, actual, "batch replay preserves rows, errors and cost: " + sql);
            Corpus.Equal(true, chunks.All(count => count is > 0 and <= 1024), "bounded nonempty host chunks");
            if (sql.Contains("CASE") || sql.Contains("v + 0")) Corpus.Equal(true, chunks.All(count => count == 1), "conditional and nontrivial calls stay scalar");
            else Corpus.Equal(true, chunks.Any(count => count > 1), "eligible held and streamed rows use batch ABI");
        }
        foreach (var maximum in new[] { 1L, 300L, 1000L, 1800L })
        {
            foreach (var sql in new[] { "SELECT batch_probe(v) FROM batch_rows ORDER BY id % 7, id", "SELECT batch_probe(v) FROM batch_rows ORDER BY id" })
                Corpus.Equal(await Read(scalarSession, sql, maximum), await Read(batchSession, sql, maximum), "batch cost abort position");
        }
        chunks.Clear();
        await Read(batchSession, "SELECT batch_probe(v) FROM batch_rows ORDER BY id LIMIT 1");
        Corpus.Equal(true, chunks.SequenceEqual([1]), "LIMIT one never reads a batch past its row");
        await PullErrors();
        await CorruptRowReplay();
        await FileReplay(batchEnvironment, scalarEnvironment, chunks);
        Console.WriteLine("PASS host batches: held and streamed rows, strict NULL, deferred source errors, scalar replay and cost ceilings");
    }
    private static async Task CorruptRowReplay()
    {
        await using var database = await Database.CreateInMemoryAsync();
        await using var pin = await database.PinAsync();
        await using var device = new MemoryBlockStore();
        var table = new CatalogTable("bad", [new("id", new(SqlType.I64), true), new("flag", new(SqlType.Boolean))], [0], RowCount: 2);
        var tree = new StorageTree(new StoragePager(device, 512), table.LeafSchema());
        for (var id = 1; id <= 2; id++)
        {
            var key = KeyEncoding.Integer(id, 8);
            tree = await tree.UpsertAsync(new(key, [key, [(byte)id]]));
        }
        var function = HostFunction.Batch("echo", [SqlType.I64], SqlType.I64, (columns, output) =>
        { foreach (var value in columns[0]) output.Add(value); }, FunctionVolatility.Immutable);
        var call = new Resolved("host", SqlType.I64, default, [new("column", SqlType.I64, default, [], Slot: 0)], HostFunction: function);
        var plan = new SelectPlan([new(call, "id")], null, null, 0, false);
        var access = new AccessPlan(AccessKind.Full, 2, 1, 0, 0);
        var storage = new QueryStorage(database, pin.Snapshot, null, pin.Snapshot.Catalog);
        async Task<Outcome> Run(bool batched, long maximum)
        {
            await using var session = new Session(); var meter = new Meter(session, maximum, default);
            using var batch = batched ? meter.HostBatches.BeginScan(plan, true) : null;
            var values = new List<string>(); string? error = null;
            try
            {
                await foreach (var row in access.Rows(table, tree, storage, meter, null, default, hostBatch: batch))
                    values.Add((await Evaluate.ExprAsync(call, meter, row: row)).ToString());
            }
            catch (SqlException failure) { error = failure.SqlState; }
            return new(string.Join(";", values), error, meter.Accrued);
        }
        foreach (var maximum in new[] { 0L, 3L, 4L })
            Corpus.Equal(await Run(false, maximum), await Run(true, maximum), "corrupt fixed cell keeps scalar row charge and guard precedence");
    }
    private static async Task FileReplay(DatabaseEnvironment batchEnvironment, DatabaseEnvironment scalarEnvironment, List<int> chunks)
    {
        var directory = Path.Combine(Path.GetTempPath(), "jed-host-batches-" + Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(directory);
        try
        {
            await using var batched = await Database.CreateFileAsync(Path.Combine(directory, "batch"), new() { Environment = batchEnvironment, Locking = FileLocking.None, WorkMem = 128 });
            await using var scalar = await Database.CreateFileAsync(Path.Combine(directory, "scalar"), new() { Environment = scalarEnvironment, Locking = FileLocking.None, WorkMem = 128 });
            await using var batchSession = batched.CreateSession();
            await using var scalarSession = scalar.CreateSession();
            foreach (var session in new[] { batchSession, scalarSession })
            {
                await session.ExecuteAsync("CREATE TABLE batch_rows (id i64 PRIMARY KEY, v i64)");
                await session.ExecuteAsync("INSERT INTO batch_rows SELECT g, g FROM generate_series(1, 240) g");
            }
            foreach (var budget in new[] { 0L, 1L })
            foreach (var sql in new[]
            {
                "SELECT batch_probe(v) FROM batch_rows ORDER BY id % 7, id",
                "SELECT batch_probe(v) FROM batch_rows ORDER BY id % 7, id LIMIT 9 OFFSET 3",
                "SELECT batch_probe(count(*)) FROM batch_rows GROUP BY id % 13",
                "SELECT batch_probe(count(*)) FROM batch_rows GROUP BY id % 13 LIMIT 2 OFFSET 3"
            })
            {
                chunks.Clear();
                var actual = await Read(batchSession, sql, memory: budget);
                var expected = await Read(scalarSession, sql, memory: budget);
                Corpus.Equal(expected, actual, "spilled batch replay preserves rows, errors and cost");
                if (budget == 0) Corpus.Equal(true, chunks.Any(count => count > 1), "sort/spool read-ahead invokes a batch");
                Corpus.Equal(true, chunks.All(count => count <= 64), "pulled batch window is bounded");
            }
            chunks.Clear();
            await using (var result = await batchSession.QueryAsync("SELECT batch_probe(v) FROM batch_rows ORDER BY id"))
                Corpus.Equal(true, await result.ReadAsync() is not null, "early streamed cursor returns first row");
            Corpus.Equal(true, chunks.Count == 1 && chunks[0] <= 64, "early close bounds speculative host work");
            await batchSession.ExecuteAsync("INSERT INTO batch_rows VALUES (241, 241)");
        }
        finally { Directory.Delete(directory, true); }
    }
    private static async Task PullErrors()
    {
        await using var session = new Session();
        var meter = new Meter(session, 0, default);
        var calls = new List<int>(); var loads = 0;
        var function = HostFunction.Batch("echo", [SqlType.I64], SqlType.I64, (columns, output) =>
        { calls.Add(columns[0].Count); foreach (var value in columns[0]) output.Add(value); }, FunctionVolatility.Immutable);
        var call = new Resolved("host", SqlType.I64, default, [new("column", SqlType.I64, default, [], Slot: 0)], HostFunction: function);
        var closed = false; var pulls = 0;
        async IAsyncEnumerable<SqlValue[]> Faulting()
        {
            await ValueTask.CompletedTask;
            try
            {
                pulls++; yield return [SqlValue.Integer(1)];
                pulls++; yield return [SqlValue.Deferred(SqlType.I64, new(_ => { loads++; return new(SqlValue.Integer(2)); }))];
                pulls++; throw new SqlException("XX001", "faulting next source leaf");
            }
            finally { closed = true; }
        }
        var window = HostPullWindow.Read(Faulting(), [call], meter, 1, () => null, default);
        await using (var rows = window.GetAsyncEnumerator())
        {
            Corpus.Equal(true, await rows.MoveNextAsync(), "source fault defers behind first row");
            Corpus.Equal(0, loads, "read-ahead does not fetch deferred cells");
            Corpus.Equal(1L, (await Evaluate.ExprAsync(call, meter, row: rows.Current)).AsInteger(), "first replay value");
            Corpus.Equal(0, loads, "host prefetch skips deferred cells");
            Corpus.Equal(true, await rows.MoveNextAsync(), "source fault defers behind second row");
            Corpus.Equal(2L, (await Evaluate.ExprAsync(call, meter, row: rows.Current)).AsInteger(), "deferred cell resolves at replay");
            Corpus.Equal(1, loads, "deferred cell loaded exactly once");
            try { await rows.MoveNextAsync(); throw new InvalidDataException("source fault lost"); }
            catch (SqlException error) when (error.SqlState == "XX001") { }
        }
        Corpus.Equal(true, closed, "failed window disposes source");
        closed = false;
        await using (var rows = HostPullWindow.Read(Faulting(), [call], meter, 1, () => null, default).GetAsyncEnumerator())
            Corpus.Equal(true, await rows.MoveNextAsync(), "early close receives first row");
        Corpus.Equal(true, closed, "early close drops deferred fault and disposes source");
        pulls = 0;
        await using (var rows = HostPullWindow.Read(Faulting(), [call], meter, 1, () => 1, default).GetAsyncEnumerator())
        {
            Corpus.Equal(true, await rows.MoveNextAsync(), "limited window first row");
            Corpus.Equal(1, pulls, "limited window does not read ahead beyond stop");
        }
        using var canceled = new CancellationTokenSource(); canceled.Cancel(); pulls = 0;
        await using (var rows = HostPullWindow.Read(Faulting(), [call], meter, 1, () => null, canceled.Token).GetAsyncEnumerator())
        {
            try { await rows.MoveNextAsync(); throw new InvalidDataException("canceled window ignored token"); }
            catch (OperationCanceledException error) when (error.CancellationToken == canceled.Token) { }
            Corpus.Equal(0, pulls, "canceled read-ahead never touches source");
        }
    }
    private static async Task<Outcome> Read(Session session, string sql, long maximum = 0, long memory = 0)
    {
        var values = new List<string>(); Rows? rows = null; string? error = null;
        var before = session.LifetimeCost;
        try
        {
            rows = await session.QueryAsync(sql, new QueryOptions { MaxCost = maximum, MaxQueryMemoryBytes = memory });
            await foreach (var row in rows) values.Add(string.Join("|", Enumerable.Range(0, row.Count).Select(i => row[i].ToString())));
        }
        catch (SqlException failure) { error = failure.SqlState; }
        finally { if (rows is not null) await rows.DisposeAsync(); }
        return new(string.Join(";", values), error, session.LifetimeCost - before);
    }
}
