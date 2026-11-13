using Jed;

// Development-only diagnostic: blank lines separate SQL statements. This runs
// the real public session and prints complete output, including EXPLAIN trees.
internal static class QueryInspect
{
    internal static async Task Run(string path)
    {
        await using var session = new Session();
        var statements = new List<string>(); var current = new List<string>();
        foreach (var line in await File.ReadAllLinesAsync(path))
        {
            if (string.IsNullOrWhiteSpace(line))
            { if (current.Count != 0) { statements.Add(string.Join('\n', current)); current.Clear(); } }
            else current.Add(line);
        }
        if (current.Count != 0) statements.Add(string.Join('\n', current));
        foreach (var sql in statements)
        {
            Console.WriteLine(sql);
            try
            {
                await using var rows = await session.QueryAsync(sql);
                Console.WriteLine(string.Join(" | ", rows.Columns.Select(column => column.Name + ":" + column.Type.Name())));
                await foreach (var row in (IAsyncEnumerable<Row>)rows)
                    Console.WriteLine(string.Join(" | ", Enumerable.Range(0, row.Count).Select(i => row[i].ToString())));
                Console.WriteLine("cost=" + rows.Cost);
            }
            catch (Exception error) when (error is ISqlError sqlError)
            { Console.WriteLine(sqlError.SqlState + ": " + error.Message); }
        }
    }
}
