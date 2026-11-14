using Jed;

internal static class PreparedCacheTests
{
    private static void Check(bool value, string reason)
    { if (!value) throw new InvalidDataException("prepared plan: " + reason); }

    private static async Task<(string Values, long Cost)> Read(Session session, PreparedStatement statement, QueryOptions? options = null)
    {
        await using var rows = await session.QueryAsync(statement, options: options);
        var values = new List<string>();
        await foreach (var row in (IAsyncEnumerable<Row>)rows)
            values.Add(string.Join("|", Enumerable.Range(0, row.Count).Select(i => row[i].ToString())));
        return (string.Join("\n", values), rows.Cost);
    }

    internal static async Task Run()
    {
        await using var database = await Database.CreateInMemoryAsync();
        await using var session = database.CreateSession();
        await session.ExecuteAsync("CREATE TABLE cache_test(id i32 PRIMARY KEY, v i64)");
        await session.ExecuteAsync("INSERT INTO cache_test VALUES (1,10),(2,20),(3,30)");
        foreach (var sql in new[] {
            "SELECT 12 * 34, NULL::i32, ARRAY[1,2]",
            "SELECT id, v FROM cache_test WHERE id=2",
            "SELECT id, v FROM cache_test WHERE id >= 2 ORDER BY id",
            "SELECT count(*), sum(v) FROM cache_test",
            "SELECT id, v FROM cache_test ORDER BY v DESC LIMIT 2",
            "SELECT id FROM cache_test ORDER BY id LIMIT 0",
            "SELECT sum(v) OVER (ORDER BY id) FROM cache_test ORDER BY id"
        })
        {
            var statement = session.Prepare(sql);
            var initial = await Read(session, statement);
            Check(await Read(session, statement) == initial, "repeat values/cost: " + sql);
            Check(await Read(session, session.Prepare(sql)) == initial, "fresh values/cost: " + sql);
            if (initial.Cost > 0)
            {
                try { await Read(session, statement, new() { MaxCost = initial.Cost }); throw new InvalidDataException("missing cached ceiling"); }
                catch (SqlException error) when (error.SqlState == "54P01") { }
            }
        }
        var point = session.Prepare("SELECT v FROM cache_test WHERE id=2");
        Check((await Read(session, point)).Values == "20", "initial point");
        await session.ExecuteAsync("UPDATE cache_test SET v=21 WHERE id=2");
        Check((await Read(session, point)).Values == "21", "commit invalidates snapshot");
        await session.ExecuteAsync("BEGIN");
        await session.ExecuteAsync("UPDATE cache_test SET v=22 WHERE id=2");
        Check((await Read(session, point)).Values == "22", "pending writes bypass cache");
        await session.ExecuteAsync("ROLLBACK");
        Check((await Read(session, point)).Values == "21", "rollback restores snapshot");
        session.Revoke("cache_test", TablePrivileges.Select);
        try { await Read(session, point); throw new InvalidDataException("cached privileges bypassed"); }
        catch (SqlException error) when (error.SqlState == "42501") { }
        session.ResetPrivileges();
        var setting = session.Prepare("SELECT current_setting('app.test')");
        session.SetVar("app.test", "first");
        Check((await Read(session, setting)).Values == "first", "initial variable");
        session.SetVar("app.test", "second");
        Check((await Read(session, setting)).Values == "second", "cached variable remains runtime state");
        await session.ExecuteAsync("CREATE SEQUENCE cache_sequence");
        await session.ExecuteAsync("SELECT nextval('cache_sequence')");
        var sequence = session.Prepare("SELECT currval('cache_sequence')");
        Check((await Read(session, sequence)).Values == "1", "initial currval");
        await session.ExecuteAsync("SELECT nextval('cache_sequence')");
        Check((await Read(session, sequence)).Values == "2", "cached currval remains runtime state");
        await using (var owned = await session.QueryAsync(session.Prepare("SELECT id, v FROM cache_test ORDER BY id")))
        {
            var first = (await owned.ReadAsync())!;
            while (await owned.ReadAsync() is not null) { }
            Check(first[0].AsInteger() == 1 && first[1].AsInteger() == 10, "rows stay owned after advancing");
        }
        var temporal = session.Prepare("SELECT '2000-01-01 00:00'::timestamptz");
        var utc = await Read(session, temporal, new() { TimeZone = "UTC" });
        var shifted = await Read(session, temporal, new() { TimeZone = "+02:00" });
        Check(utc.Values != shifted.Values, "timezone participates in cache signature");
        Check(await Read(session, temporal, new() { TimeZone = "UTC" }) == utc, "timezone restoration");
        await using var other = new Session();
        await other.ExecuteAsync("CREATE TABLE cache_test(id i32 PRIMARY KEY, v text)");
        await other.ExecuteAsync("INSERT INTO cache_test VALUES (2,'other database')");
        Check((await Read(other, point)).Values == "other database", "prepared statement reuse across sessions/databases");
        await session.ExecuteAsync("DROP TABLE cache_test");
        await session.ExecuteAsync("CREATE TABLE cache_test(id i32 PRIMARY KEY, v text)");
        await session.ExecuteAsync("INSERT INTO cache_test VALUES (2,'new schema')");
        Check((await Read(session, point)).Values == "new schema", "drop/recreate invalidation");
        Console.WriteLine("PASS prepared plan reuse: rows/costs, ceilings, pending writes, privileges, timezone and schema/database invalidation");
    }
}
