using Jed;

internal static class ParameterHostTests
{
    internal static async Task Run()
    {
        PreparedStatement prepared;
        await using (var preparing = new Session()) prepared = preparing.Prepare("SELECT $1::i32 + 1");
        async Task<long> Execute(long value)
        {
            await using var session = new Session();
            await using var rows = await session.QueryAsync(prepared, [SqlValue.Integer(value)]);
            var row = await rows.ReadAsync();
            if (row is null || row[0].Type != SqlType.I32) throw new Exception("prepared statement metadata mismatch");
            var result = row[0].AsInteger();
            if (await rows.ReadAsync() is not null) throw new Exception("prepared statement produced extra row");
            return result;
        }
        var results = await Task.WhenAll(Execute(10), Execute(20));
        if (!results.SequenceEqual([11L, 21L])) throw new Exception("prepared executions shared bound parameter state");
        await using var limited = new Session(new SessionOptions { MaxSqlLength = 3 });
        try { limited.Prepare("SELECT 1"); throw new Exception("prepare ignored input limit"); }
        catch (SqlException error) when (error.SqlState == "54000") { }
        await using var session2 = new Session();
        try { session2.Prepare("SELECT ("); throw new Exception("prepare deferred parse failure"); }
        catch (SqlException error) when (error.SqlState == "42601") { }
        var insert = session2.Prepare("INSERT INTO t VALUES ($1)");
        await using (var create = await session2.QueryAsync("CREATE TABLE t (k i32 PRIMARY KEY)"))
            while (await create.ReadAsync() is not null) { }
        if (await session2.ExecuteAsync(insert, [SqlValue.Integer(4)]) != 1) throw new Exception("prepared command affected count");
        Console.WriteLine("PASS prepared host API: detached parse, concurrent independent bindings, parse limits and command result");
    }
}
