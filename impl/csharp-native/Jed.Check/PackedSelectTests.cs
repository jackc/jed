using Jed;

internal static class PackedSelectTests
{
    internal static async Task Run()
    {
        var table = new CatalogTable("fixed", [new("id", new(SqlType.I32)), new("v", new(SqlType.I64)), new("flag", new(SqlType.Boolean))], [0]);
        var records = Enumerable.Range(1, 12).Select(i => table.EncodeRow([SqlValue.Integer(i, SqlType.I32),
            i == 4 ? SqlValue.Null(SqlType.I64) : SqlValue.Integer(i), SqlValue.Boolean(i % 2 == 0)])).ToArray();
        var schema = table.LeafSchema();
        await using var device = new MemoryBlockStore();
        await device.SetLengthAsync(3 * 4096); await device.WriteAsync(2 * 4096, StorageLeaf.Encode(4096, schema, records));
        var pager = new StoragePager(device, 4096);
        var id = new Resolved("column", SqlType.I32, default, [], Slot: 0);
        var value = new Resolved("column", SqlType.I64, default, [], Slot: 1);
        var seven = new Resolved("constant", SqlType.I64, SqlValue.Integer(7), []);
        var divisor = new Resolved("-", SqlType.I64, default, [value, seven]);
        var division = new Resolved("/", SqlType.I64, default, [seven, divisor]);
        var failingFilter = new Resolved(">", SqlType.Boolean, default, [division, new("constant", SqlType.I64, SqlValue.Integer(0), [])]);
        var normalFilter = new Resolved(">", SqlType.Boolean, default, [value, new("constant", SqlType.I64, SqlValue.Integer(2), [])]);
        Projection[] identity = [new(id, "id"), new(value, "v"), new(value, "again")];
        Projection[] expression = [new(id, "id"), new(new("+", SqlType.I64, default, [value, seven]), "v")];
        foreach (var resident in new[] { false, true })
        foreach (var reverse in new[] { false, true })
        foreach (var shape in Enumerable.Range(0, 7))
        foreach (var scenario in Enumerable.Range(0, 9))
        {
            var tree = new StorageTree(pager, schema, shape == 6 ? 0u : 2u, 3, residentRoot: resident && shape != 6 ? records : null);
            var columnar = scenario is 0 or 1 or 7;
            var point = shape == 2 ? KeyEncoding.Integer(99, 4) : records[6].Key;
            var scan = shape == 0 ? new TableScan(records[0].Key, false, records[^1].Key, false, reverse)
                : new TableScan(point, shape != 3, point, shape != 4, reverse, Empty: shape == 5);
            Projection[] allColumns = [new(id, "id"), new(value, "v"), new(new("column", SqlType.Boolean, default, [], Slot: 2), "flag")];
            var plan = new SelectPlan(scenario is 7 or 8 ? allColumns : columnar ? identity : scenario == 3 ? [new(division, "fail")] : expression,
                scenario is 1 or 2 ? failingFilter : normalFilter, columnar ? null : scenario == 4 ? 0 : scenario == 5 ? 2 : null,
                scenario == 5 ? 2 : 0, scenario == 6, Access: new(AccessKind.Primary, 10, 1, 0, 1, Scan: scan),
                ColumnarProjection: columnar, ScanReverse: reverse);
            plan = plan with { PackedSelect = PackedSelectInput.Create(plan, table, tree) };
            Corpus.Equal(true, plan.PackedSelect is not null, "simple fixed scan admits fused pipeline");
            async Task<(string Rows, string? Error, long Cost, long Peak, long Used)> Check(bool fused, long ceiling = 0, long memory = 0, int cancelAt = -1, int stopAt = -1, bool separateToken = false)
            {
                await using var session = new Session();
                using var cancellation = new CancellationTokenSource(); if (cancelAt == 0) cancellation.Cancel();
                var meter = new Meter(session, ceiling, separateToken ? default : cancellation.Token, new QueryOptions { MaxQueryMemoryBytes = memory });
                var current = fused ? plan : plan with { PackedSelect = null };
                var rows = new List<string>(); string? error = null;
                try
                {
                    var source = current.Access!.Rows(table, tree, null!, meter, null, cancellation.Token, reverse);
                    await foreach (var row in QueryExecution.Select(current, source, meter, cancellation.Token))
                    {
                        rows.Add(string.Join("|", row.OwnedValues.Select(cell => Convert.ToHexString(StoredValues.Encode(cell)))));
                        if (rows.Count == cancelAt) cancellation.Cancel();
                        if (rows.Count == stopAt) break;
                    }
                }
                catch (SqlException exception) { error = exception.SqlState; }
                catch (OperationCanceledException) { error = "canceled"; }
                return (string.Join(";", rows), error, meter.Accrued, meter.Memory.Peak, meter.Memory.Used);
            }
            var expected = await Check(false);
            Corpus.Equal(expected, await Check(true), "fused scan preserves rows, filter/project error order, cost and logical memory");
            for (var ceiling = 1L; ceiling <= expected.Cost + 1; ceiling++)
                Corpus.Equal(await Check(false, ceiling), await Check(true, ceiling), "fused scan exact cost guard boundary");
            foreach (var memory in new long[] { 1, 50, 200, 10000 })
                Corpus.Equal(await Check(false, memory: memory), await Check(true, memory: memory), "blocking columnar memory admission follows full filtering and precedes projection");
            foreach (var cancelAt in new[] { 0, 1, 3 })
            {
                Corpus.Equal(await Check(false, cancelAt: cancelAt), await Check(true, cancelAt: cancelAt), "fused scan cancellation after public row");
                Corpus.Equal(await Check(false, cancelAt: cancelAt, separateToken: true), await Check(true, cancelAt: cancelAt, separateToken: true), "explicit scan token remains distinct from expression meter token");
            }
            Corpus.Equal(await Check(false, stopAt: 1), await Check(true, stopAt: 1), "early consumer disposal releases columnar reservation");
        }
        await IdentityAllocation();
        await CachedSnapshots();
        Console.WriteLine("PASS fused fixed scans: forced generic equivalence, blocking admission, all guard cutoffs, cancellation and early disposal");
    }

    private static async Task CachedSnapshots()
    {
        await using var database = await Database.CreateInMemoryAsync();
        await using var session = database.CreateSession();
        await session.ExecuteAsync("CREATE TABLE fused_cache(id i32 PRIMARY KEY, v i64)");
        await session.ExecuteAsync("INSERT INTO fused_cache VALUES (1,10),(2,20),(3,30)");
        var statements = new[] { session.Prepare("SELECT id, v FROM fused_cache WHERE id >= 1 ORDER BY id"),
            session.Prepare("SELECT id, v + 1 FROM fused_cache WHERE id >= 1 ORDER BY id") };
        var retained = new List<(Row Row, string Text)>();
        async Task<(string Rows, long Cost)> Read(PreparedStatement statement)
        {
            await using var result = await session.QueryAsync(statement);
            var rows = new List<string>();
            await foreach (var row in (IAsyncEnumerable<Row>)result)
            {
                var text = $"{row[0]}:{row[1]}"; rows.Add(text); retained.Add((row, text));
                foreach (var previous in retained)
                    Corpus.Equal(previous.Text, $"{previous.Row[0]}:{previous.Row[1]}", "owned projected rows remain stable across advancement and cached execution");
            }
            return (string.Join(";", rows), result.Cost);
        }
        var initial = new (string Rows, long Cost)[statements.Length];
        for (var i = 0; i < statements.Length; i++)
        {
            initial[i] = await Read(statements[i]);
            Corpus.Equal(initial[i], await Read(statements[i]), "fused repeated cached execution");
        }
        await session.ExecuteAsync("BEGIN");
        await session.ExecuteAsync("UPDATE fused_cache SET v=99 WHERE id=2");
        for (var i = 0; i < statements.Length; i++)
            Corpus.Equal(true, (await Read(statements[i])).Rows.Contains(i == 0 ? "2:99" : "2:100", StringComparison.Ordinal), "pending state bypasses retained fused snapshot");
        await session.ExecuteAsync("ROLLBACK");
        for (var i = 0; i < statements.Length; i++) Corpus.Equal(initial[i], await Read(statements[i]), "rollback restores cached fused snapshot");
        await session.ExecuteAsync("UPDATE fused_cache SET v=77 WHERE id=2");
        for (var i = 0; i < statements.Length; i++)
        {
            var committed = await Read(statements[i]);
            Corpus.Equal(true, committed.Rows.Contains(i == 0 ? "2:77" : "2:78", StringComparison.Ordinal), "commit invalidates fused snapshot");
            Corpus.Equal(committed, await Read(statements[i]), "updated fused snapshot retained consistently");
        }
        foreach (var previous in retained)
            Corpus.Equal(previous.Text, $"{previous.Row[0]}:{previous.Row[1]}", "retained public rows survive cursor disposal and snapshot changes");
    }

    private static async Task IdentityAllocation()
    {
        await using var session = new Session();
        var meter = new Meter(session, 0, default);
        Projection[] projections = [new(new("column", SqlType.I64, default, [], Slot: 0), "a"), new(new("column", SqlType.I64, default, [], Slot: 1), "b")];
        SqlValue[] values = [SqlValue.Integer(12), SqlValue.Integer(34)];
        var copied = QueryExecution.Project(projections, values, meter);
        var transferred = QueryExecution.Project(projections, values, meter, ownedIdentity: true);
        Corpus.Equal(false, ReferenceEquals(values, copied.OwnedValues), "ordinary projection owns independent array");
        Corpus.Equal(true, ReferenceEquals(values, transferred.OwnedValues), "identity projection transfers owned resident array");
        const int count = 1000;
        var before = GC.GetAllocatedBytesForCurrentThread();
        for (var i = 0; i < count; i++) GC.KeepAlive(QueryExecution.Project(projections, values, meter));
        var copiedBytes = GC.GetAllocatedBytesForCurrentThread() - before;
        before = GC.GetAllocatedBytesForCurrentThread();
        for (var i = 0; i < count; i++) GC.KeepAlive(QueryExecution.Project(projections, values, meter, ownedIdentity: true));
        var ownedBytes = GC.GetAllocatedBytesForCurrentThread() - before;
        Corpus.Equal(true, copiedBytes - ownedBytes >= count * 48, "owned identity removes at least the scalar payload allocation");
    }
}
