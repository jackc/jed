using Jed;

internal static class HostTests
{
    internal static async Task Run()
    {
        await using var session = new Session();
        var pending = session.QueryAsync("SELECT 7 AS n");
        Corpus.Equal(true, pending.IsCompletedSuccessfully, "resident query completes inline");
        await using var rows = await pending;
        var read = rows.ReadAsync();
        Corpus.Equal(true, read.IsCompletedSuccessfully, "resident row completes inline");
        var owned = (await read)!;
        Corpus.Equal(7L, owned[0].AsInteger(), "owned value");
        Corpus.Equal<Row?>(null, await rows.ReadAsync(), "end of cursor");
        await rows.DisposeAsync();
        Corpus.Equal(7L, owned[0].AsInteger(), "value survives cursor disposal");
        Corpus.Equal(1L, rows.Cost, "final cursor cost");

        await using var failing = await session.QueryAsync("SELECT 1 / 0");
        await Error(SqlStates.DivisionByZero, async () => { _ = await failing.ReadAsync(); });
        Corpus.Equal(2L, failing.Cost, "drain-time error cost");
        await using var limited = await session.QueryAsync("SELECT 1 + 2", maxCost: 2);
        await Error(SqlStates.CostLimitExceeded, async () => { _ = await limited.ReadAsync(); });
        Corpus.Equal(2L, limited.Cost, "cost abort checkpoint");

        await using var lifetime = new Session(lifetimeMaxCost: 2);
        await using (var first = await lifetime.QueryAsync("SELECT 1")) { _ = await first.ReadAsync(); }
        await using (var second = await lifetime.QueryAsync("SELECT 2"))
            await Error(SqlStates.SessionCostLimitExceeded, async () => { _ = await second.ReadAsync(); });
        Corpus.Equal(2L, lifetime.LifetimeCost, "failed statement retained in lifetime cost");

        using var cancellation = new CancellationTokenSource();
        await using var canceled = await session.QueryAsync("SELECT 1", cancellationToken: cancellation.Token);
        cancellation.Cancel();
        await Error(SqlStates.QueryCanceled, async () => { _ = await canceled.ReadAsync(); });
        Corpus.Equal(0L, canceled.Cost, "cancellation before work");
        var canceledQuery = session.QueryAsync("SELECT 1", cancellationToken: cancellation.Token);
        await Error(SqlStates.QueryCanceled, async () => { _ = await canceledQuery; });
        Corpus.Equal(true, canceledQuery.IsCanceled, "task cancellation state");
        await using var iterationCancellation = await session.QueryAsync("SELECT 1");
        var canceledRead = iterationCancellation.ReadAsync(cancellation.Token);
        Corpus.Equal(true, canceledRead.IsCanceled, "ValueTask cancellation state");
        await Error(SqlStates.QueryCanceled, async () => { _ = await canceledRead; });

        await Error(SqlStates.CharacterNotInRepertoire, () => { _ = SqlValue.Text("\ud800"); return Task.CompletedTask; });
        await Error(SqlStates.NullValueNotAllowed, () => { _ = SqlValue.Null(SqlType.I32).AsInteger(); return Task.CompletedTask; });
        await Error(SqlStates.DatatypeMismatch, () => { _ = SqlValue.Boolean(true).AsInteger(); return Task.CompletedTask; });

        // Parser safety is outside finite SQL fixture reach at practical attack sizes.
        await Error(SqlStates.StatementTooComplex, async () => { _ = await session.QueryAsync("SELECT " + new string('(', 10_000) + "1" + new string(')', 10_000)); });
        await Error(SqlStates.StatementTooComplex, async () => { _ = await session.QueryAsync("SELECT " + string.Join("+", Enumerable.Repeat("1", 10_000))); });
        await using (var nesting = await session.QueryAsync("SELECT " + new string('(', 256) + "1" + new string(')', 256)))
            Corpus.Equal(1L, (await nesting.ReadAsync())![0].AsInteger(), "maximum parentheses nesting");
        await using (var unary = await session.QueryAsync("SELECT " + string.Concat(Enumerable.Repeat("NOT ", 256)) + "true"))
            Corpus.Equal(true, (await unary.ReadAsync())![0].AsBoolean(), "maximum unary nesting");
        await Error(SqlStates.StatementTooComplex, async () => { _ = await session.QueryAsync("SELECT " + string.Concat(Enumerable.Repeat("NOT ", 257)) + "true"); });
        await Error(SqlStates.NameTooLong, async () => { _ = await session.QueryAsync("SELECT 1 AS \"" + new string('é', 32) + "\""); });

        await using var partial = await session.QueryAsync("SELECT 1 / 0");
        await partial.DisposeAsync();
        Corpus.Equal(0L, partial.Cost, "closing unread cursor performs no evaluation");

        // Host locale must not alter integer formatting or SQL name matching.
        var saved = System.Globalization.CultureInfo.CurrentCulture;
        try
        {
            System.Globalization.CultureInfo.CurrentCulture = new("tr-TR");
            await using var culture = await session.QueryAsync("SELECT CAST(-123 AS INTEGER) AS ID");
            Corpus.Equal("id", culture.Columns[0].Name, "ASCII name folding");
            Corpus.Equal("-123", (await culture.ReadAsync())![0].ToString(), "invariant integer render");
        }
        finally { System.Globalization.CultureInfo.CurrentCulture = saved; }
        Console.WriteLine("PASS host API: inline completion, owned rows, drain errors, cost/lifetime limits, cancellation, Unicode, nesting, disposal and locale");
    }

    private static async Task Error(string expected, Func<Task> action)
    {
        try { await action(); }
        catch (Exception error) when (error is ISqlError sql)
        {
            Corpus.Equal(expected, sql.SqlState, "SQLSTATE"); return;
        }
        throw new InvalidDataException($"expected SQLSTATE {expected}, got success");
    }
}
