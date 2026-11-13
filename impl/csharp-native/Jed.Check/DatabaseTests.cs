using Jed;

internal static class DatabaseTests
{
    internal static async Task Run()
    {
        await using var database = await Database.CreateInMemoryAsync(pageSize: 256);
        await using var writer = database.CreateSession();
        await using var reader = database.CreateSession();
        await Exec(writer, "CREATE TABLE accounts (id i64 PRIMARY KEY, balance numeric(12,2) NOT NULL, label text DEFAULT 'open')");
        await Exec(writer, "INSERT INTO accounts (id,balance) VALUES (1,12.25),(2,30.00),(3,40.50)");
        await using var frozen = await reader.QueryAsync("SELECT id,balance,label FROM accounts ORDER BY id");
        await Exec(writer, "BEGIN");
        await Exec(writer, "UPDATE accounts SET balance=balance+5.25 WHERE id=2");
        Corpus.Equal("30.00", await Scalar(reader, "SELECT balance FROM accounts WHERE id=2"), "uncommitted writes invisible");
        Corpus.Equal("35.25", await Scalar(writer, "SELECT balance FROM accounts WHERE id=2"), "writer sees pending rows");
        await Exec(writer, "COMMIT");
        Corpus.Equal("35.25", await Scalar(reader, "SELECT balance FROM accounts WHERE id=2"), "committed root publication");
        var frozenRows = new List<string>();
        await foreach (var row in frozen) frozenRows.Add(string.Join('|', Enumerable.Range(0, row.Count).Select(i => row[i].ToString())));
        Corpus.Equal("1|12.25|open;2|30.00|open;3|40.50|open", string.Join(';', frozenRows), "cursor owns pre-commit snapshot");
        await Exec(writer, "BEGIN");
        await HostTests.Error(SqlStates.UniqueViolation, () => Exec(writer, "INSERT INTO accounts (id,balance) VALUES (4,1),(1,2)"));
        await HostTests.Error(SqlStates.InFailedSqlTransaction, () => Exec(writer, "SELECT 1"));
        await Exec(writer, "ROLLBACK");
        Corpus.Equal<string?>(null, await Scalar(reader, "SELECT id FROM accounts WHERE id=4"), "failed statement discarded all staged rows");
        await Exec(writer, "INSERT INTO accounts (id,balance) VALUES (4,1),(5,2)");
        await Exec(writer, "DELETE FROM accounts WHERE id=1");
        Corpus.Equal("5", await Scalar(reader, "SELECT id FROM accounts ORDER BY id DESC LIMIT 1"), "descending paged query");
        await Exec(writer, "CREATE TABLE bag (n i32)");
        await Exec(writer, "INSERT INTO bag VALUES (1),(1),(2)");
        await Exec(writer, "UPDATE bag SET n=n+1");
        var bag = new List<string>();
        await using (var cursor = await reader.QueryAsync("SELECT n FROM bag ORDER BY n"))
            await foreach (var row in cursor) bag.Add(row[0].ToString());
        Corpus.Equal("2,2,3", string.Join(',', bag), "synthetic row keys preserve duplicate rows");
        await Exec(writer, "DROP TABLE bag");
        await HostTests.Error(SqlStates.UndefinedTable, () => Exec(reader, "SELECT * FROM bag"));
        await Retirement(database, writer);
        Console.WriteLine("PASS database host: paged rows, snapshot publication, transaction rollback, synthetic row keys");
    }
    private static async Task Retirement(Database database, Session writer)
    {
        await Exec(writer, "CREATE TABLE recycled (n i32 PRIMARY KEY)");
        await Exec(writer, "INSERT INTO recycled VALUES (1),(2),(3)");
        using var retained = database.Pin();
        var retainedTable = retained.Snapshot.Catalog.Tables.Single(table => table.Name == "recycled");
        var retainedTree = database.Tree(retainedTable, null, retained.Snapshot);
        await Exec(writer, "BEGIN");
        await Exec(writer, "DROP TABLE recycled");
        await Exec(writer, "CREATE TABLE recycled (label text PRIMARY KEY)");
        await Exec(writer, "INSERT INTO recycled VALUES ('new')");
        await Exec(writer, "COMMIT");
        for (var i = 0; i < 35; i++)
        {
            await Exec(writer, "UPDATE accounts SET balance=balance+1 WHERE id=2");
            using var current = database.Pin();
            long live = current.Snapshot.Catalog.Pages.Length;
            foreach (var table in current.Snapshot.Catalog.Tables)
                live += await database.Tree(table, null, current.Snapshot).CountPagesAsync();
            Corpus.Equal((uint)live, current.Snapshot.Meta.LivePages, "live pages equal reachable catalog and tree nodes after retirement/reuse");
        }
        var rows = new List<long>();
        await foreach (var record in retainedTree.ScanAsync()) rows.Add(KeyEncoding.ReadInteger(record.Key));
        Corpus.Equal("1,2,3", string.Join(',', rows), "reader pin protects dropped tree across many page reuses");
        Corpus.Equal("new", await Scalar(writer, "SELECT label FROM recycled"), "drop/recreate same name keeps new schema and data");
        retained.Dispose();
        uint maximum = 0;
        for (var i = 0; i < 100; i++)
        {
            await Exec(writer, "UPDATE accounts SET balance=balance+1 WHERE id=2");
            using var current = database.Pin(); maximum = Math.Max(maximum, current.Snapshot.Meta.PageCount);
        }
        Corpus.Equal(true, maximum < 160, "housekeeping and retired tree page reuse bounds churn growth");
    }

    private static async Task Exec(Session session, string sql)
    {
        await using var rows = await session.QueryAsync(sql);
        await foreach (var unused in rows) { }
    }
    private static async Task<string?> Scalar(Session session, string sql)
    {
        await using var rows = await session.QueryAsync(sql);
        var row = await rows.ReadAsync(); return row?[0].ToString();
    }
}
