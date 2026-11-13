using Jed;

internal static class HostFunctionTests
{
    internal static async Task Run()
    {
        var calls = 0;
        var builder = DatabaseEnvironment.Builder()
            .RegisterFunction(new("twice", [SqlType.I64], SqlType.I64, values => { calls++; return SqlValue.Integer(values[0].AsInteger() * 2); })
            { Cost = 7, Volatility = FunctionVolatility.Immutable, ComponentId = "test", SemanticVersion = 1 })
            .RegisterFunction(new("wrong", [], SqlType.I64, _ => SqlValue.Text("wrong")))
            .RegisterFunction(new("wide", [], SqlType.I16, _ => SqlValue.Integer(40000)))
            .RegisterFunction(new("decimal_arg", [SqlType.Decimal], SqlType.Decimal, values => values[0]))
            .RegisterFunction(new("abs", [SqlType.I64], SqlType.I64, _ => SqlValue.Integer(999)))
            .RegisterFunction(HostFunction.Batch("missing", [SqlType.I64], SqlType.I64, (_, _) => { }));
        var environment = builder.Build();
        builder.RegisterFunction(new("later", [], SqlType.I64, _ => SqlValue.Integer(1)));
        await using var database = await Database.CreateInMemoryAsync(environment: environment);
        await using var session = database.CreateSession();
        Corpus.Equal(6L, (await session.QueryFirstOrDefaultAsync("SELECT twice(3)"))![0].AsInteger(), "host scalar invocation");
        Corpus.Equal(1, calls, "scalar once");
        Corpus.Equal(true, (await session.QueryFirstOrDefaultAsync("SELECT twice(NULL::i64)"))![0].IsNull, "host strict NULL");
        Corpus.Equal(1, calls, "NULL does not invoke kernel");
        await Error("42883", () => session.ExecuteAsync("SELECT later()"));
        await Error("22000", () => session.ExecuteAsync("SELECT wrong()"));
        await Error("22003", () => session.ExecuteAsync("SELECT wide()"));
        await Error("22000", () => session.ExecuteAsync("SELECT missing(1)"));
        Corpus.Equal("2", (await session.QueryFirstOrDefaultAsync("SELECT decimal_arg(2)"))![0].ToString(), "host argument widening");
        Corpus.Equal(3L, (await session.QueryFirstOrDefaultAsync("SELECT abs(-3::i64)"))![0].AsInteger(), "builtin exact wins");
        try { await using var result = await session.QueryAsync("SELECT twice(1)", new QueryOptions { MaxCost = 6 }); await foreach (var _ in result) { } throw new InvalidDataException("host cost guard missing"); }
        catch (SqlException error) when (error.SqlState == SqlStates.CostLimitExceeded) { }
        Corpus.Equal(1, calls, "cost ceiling before host callback");
        await session.ExecuteAsync("CREATE TABLE h (id i64 PRIMARY KEY, v i64, CONSTRAINT positive CHECK (twice(v) > 0))");
        await session.ExecuteAsync("CREATE INDEX h_twice ON h ((twice(v)))");
        await session.ExecuteAsync("INSERT INTO h VALUES (1, 3)");
        Corpus.Equal(3L, (await session.QueryFirstOrDefaultAsync("SELECT v FROM h WHERE twice(v) = 6"))![0].AsInteger(), "persisted host index");
        await Error("23514", () => session.ExecuteAsync("INSERT INTO h VALUES (2, -1)"));
        await using var pin = await database.PinAsync();
        var table = pin.Snapshot.Catalog.Tables.Single(table => table.Name == "h");
        Corpus.Equal(1, table.Checks![0].HostDependencies!.Length, "check pins resolved host signature");
        Corpus.Equal(1, table.Indexes![0].HostDependencies!.Length, "index pins resolved host signature");
        await using (var restricted = database.CreateSession())
        {
            restricted.RevokeExecute("twice");
            await Error("42501", () => restricted.ExecuteAsync("SELECT twice(1)"));
        }
        var image = await database.SerializeAsync();
        await using (var missing = await Database.DeserializeAsync(image))
        {
            await using var reader = missing.CreateSession();
            Corpus.Equal(3L, (await reader.QueryFirstOrDefaultAsync("SELECT v FROM h"))![0].AsInteger(), "missing host index does not block heap reads");
            await Error("42883", () => reader.ExecuteAsync("UPDATE h SET v = 4"));
        }
        var replacement = DatabaseEnvironment.Builder().RegisterFunction(new("twice", [SqlType.I64], SqlType.I64,
            values => SqlValue.Integer(values[0].AsInteger() * 3)) { Volatility = FunctionVolatility.Immutable, ComponentId = "test", SemanticVersion = 2 }).Build();
        await using (var changed = await Database.DeserializeAsync(image, new DatabaseDeserializeOptions { Environment = replacement }))
        {
            await using var writer = changed.CreateSession();
            Corpus.Equal(3L, (await writer.QueryFirstOrDefaultAsync("SELECT v FROM h WHERE twice(v) = 9"))![0].AsInteger(), "stale host index falls back to current heap evaluation");
            await Error("XX002", () => writer.ExecuteAsync("INSERT INTO h VALUES (2, 4)"));
            await writer.ExecuteAsync("REINDEX INDEX h_twice");
            await Error("XX002", () => writer.ExecuteAsync("UPDATE h SET v = 4"));
            await writer.ExecuteAsync("ALTER TABLE h DROP CONSTRAINT positive, ADD CONSTRAINT positive CHECK (twice(v) > 0)");
            await writer.ExecuteAsync("UPDATE h SET v = 4");
            Corpus.Equal(4L, (await writer.QueryFirstOrDefaultAsync("SELECT v FROM h WHERE twice(v) = 12"))![0].AsInteger(), "atomic index and check rebuild adopts new version");
        }
        await HostBatchTests.Run();
        Console.WriteLine("PASS host functions: strict scalar dispatch, declared cost, checked results, overloads and stored dependency pins");
    }
    private static async Task Error(string state, Func<Task<long?>> action)
    {
        try { await action(); throw new InvalidDataException("expected SQLSTATE " + state); }
        catch (SqlException error) when (error.SqlState == state) { }
    }
}
