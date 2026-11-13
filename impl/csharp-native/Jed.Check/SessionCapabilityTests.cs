using Jed;

internal static class SessionCapabilityTests
{
    internal static async Task Run()
    {
        await using var database = await Database.CreateInMemoryAsync();
        await using var first = database.CreateSession();
        await using var second = database.CreateSession();
        await Execute(first, "CREATE TABLE t (id i32 PRIMARY KEY)");
        await Execute(first, "INSERT INTO t VALUES (1)");
        first.SetVar("App.Tenant", "before");
        Corpus.Equal("before", first.Var("APP.TENANT"), "host variable ASCII folding");
        Corpus.Equal<string?>(null, second.Var("app.tenant"), "host variable isolation");
        await HostTests.Error("42704", () => { first.SetVar("unknown", "x"); return Task.CompletedTask; });
        await HostTests.Error("42704", () => { first.ResetVar("unknown"); return Task.CompletedTask; });
        await using (var frozen = await first.QueryAsync("SELECT current_setting('app.tenant')"))
        {
            first.SetVar("app.tenant", "after");
            Corpus.Equal("before", (await frozen.ReadAsync())![0].AsText(), "cursor freezes variable envelope");
        }
        await Execute(first, "BEGIN"); first.SetVar("app.tenant", "survives"); await Execute(first, "ROLLBACK");
        Corpus.Equal("survives", first.Var("app.tenant"), "session variables do not roll back");
        await using (var permitted = await first.QueryAsync("SELECT id FROM t"))
        {
            first.Revoke("t", TablePrivileges.Select);
            Corpus.Equal(1L, (await permitted.ReadAsync())![0].AsInteger(), "cursor freezes privilege envelope");
        }
        await HostTests.Error("42501", async () => { await using var denied = await first.QueryAsync("SELECT id FROM t"); });
        first.Grant("t", TablePrivileges.Select);
        await HostTests.Error("42501", async () => { await using var denied = await first.QueryAsync("SELECT id FROM t"); });
        await Execute(second, "SELECT id FROM t");
        first.ResetPrivileges(); await Execute(first, "SELECT id FROM t");
        await using (var read = await database.OpenReadSessionAsync())
        {
            await HostTests.Error("25006", async () => { await using var denied = await read.QueryAsync("INSERT INTO t VALUES (2)"); });
            await Execute(read, "SELECT id FROM t");
            await HostTests.Error("25006", async () => { await using var denied = await read.QueryAsync("COMMIT"); });
        }
        Console.WriteLine("PASS session host capabilities: isolated state, deny precedence, frozen cursor envelopes, variable lifetime and immutable read handles");
    }
    private static async Task Execute(Session session, string sql)
    {
        await using var rows = await session.QueryAsync(sql);
        while (await rows.ReadAsync() is not null) { }
    }
}
