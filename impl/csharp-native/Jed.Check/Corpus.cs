using System.Globalization;
using System.Text.RegularExpressions;
using Jed;

internal sealed record CorpusRecord(int Line, string Header, string Sql, string[] Expected, Dictionary<string, string> Directives);

internal static class Corpus
{
    internal static List<CorpusRecord> Parse(string file)
    {
        var lines = File.ReadAllLines(file);
        var records = new List<CorpusRecord>();
        var pending = new Dictionary<string, string>(StringComparer.Ordinal);
        for (var i = 0; i < lines.Length;)
        {
            var line = lines[i].Trim();
            if (line.Length == 0) { i++; continue; }
            if (line.StartsWith('#'))
            {
                var match = Regex.Match(line, "^# ([a-z_]+): (.*)$", RegexOptions.CultureInvariant);
                if (match.Success)
                {
                    var key = match.Groups[1].Value;
                    if (key != "requires")
                    {
                        if (key is not ("cost" or "types" or "names" or "max_cost" or "lifetime_max_cost"))
                            throw new InvalidDataException($"{file}:{i + 1}: unsupported harness directive {key}");
                        if (!pending.TryAdd(key, match.Groups[2].Value)) throw new InvalidDataException($"duplicate directive {key}");
                    }
                }
                i++; continue;
            }
            var lineNumber = i + 1;
            var header = line;
            if (!header.StartsWith("query ", StringComparison.Ordinal) && !header.StartsWith("statement ", StringComparison.Ordinal))
                throw new InvalidDataException($"{file}:{lineNumber}: unsupported record {header}");
            i++;
            var sql = new List<string>();
            while (i < lines.Length && lines[i].Length > 0 && lines[i] != "----") sql.Add(lines[i++]);
            var expected = new List<string>();
            if (header.StartsWith("query ", StringComparison.Ordinal))
            {
                if (i >= lines.Length || lines[i] != "----") throw new InvalidDataException("query is missing ----");
                i++;
                while (i < lines.Length && lines[i].Length > 0) expected.Add(lines[i++]);
            }
            if (sql.Count == 0) throw new InvalidDataException("empty SQL record");
            records.Add(new(lineNumber, header, string.Join('\n', sql), expected.ToArray(), pending));
            pending = new(StringComparer.Ordinal);
        }
        if (pending.Count > 0) throw new InvalidDataException("dangling harness directives");
        if (records.Count == 0) throw new InvalidDataException("empty corpus file");
        return records;
    }

    internal static async Task<int> Run(string file)
    {
        var records = Parse(file);
        await using var session = new Session();
        foreach (var record in records)
        {
            try { await RunRecord(session, record).ConfigureAwait(false); }
            catch (Exception error) { throw new InvalidDataException($"{file}:{record.Line}: {record.Sql}\n{error.Message}", error); }
        }
        Console.WriteLine($"PASS {file}: {records.Count} records");
        return records.Count;
    }

    private static async Task RunRecord(Session session, CorpusRecord record)
    {
        // Lifetime directives need persistent session option mutation, not a
        // throwaway replacement session. Reject until that contract is implemented.
        if (record.Directives.ContainsKey("lifetime_max_cost")) throw new InvalidDataException("lifetime_max_cost directive is not implemented");
        var fields = record.Header.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var query = fields[0] == "query";
        if (query && (fields.Length != 3 || fields[1].Any(c => !"IBT".Contains(c))))
            throw new InvalidDataException("unsupported query header or render tag");
        if (!query && !(fields is ["statement", "ok"] || fields is ["statement", "error", _]))
            throw new InvalidDataException("invalid statement header");
        if (!query && (record.Directives.ContainsKey("names") || record.Directives.ContainsKey("types")))
            throw new InvalidDataException("names/types directives require a query record");
        var expectedError = !query && fields[1] == "error" ? fields[2] : null;
        var cells = new List<string>();
        Rows? rows = null;
        ISqlError? failure = null;
        try
        {
            var ceiling = record.Directives.TryGetValue("max_cost", out var max) ? long.Parse(max, CultureInfo.InvariantCulture) : 0;
            rows = await session.QueryAsync(record.Sql, ceiling).ConfigureAwait(false);
            if (query)
            {
                Equal(fields[1].Length, rows.Columns.Count, "column count");
                if (record.Directives.TryGetValue("types", out var types))
                    EqualList(Split(types), rows.Columns.Select(c => c.Type.Name()).ToArray(), "types");
                if (record.Directives.TryGetValue("names", out var names))
                    EqualList(Split(names), rows.Columns.Select(c => c.Name).ToArray(), "names");
            }
            // Drain through the public async API. Execution errors and final
            // costs can occur here; QueryAsync success alone is not a pass.
            await foreach (var row in ((IAsyncEnumerable<Row>)rows).ConfigureAwait(false))
                for (var i = 0; i < row.Count; i++) cells.Add(row[i].ToString());
        }
        catch (Exception error) when (error is ISqlError sqlError) { failure = sqlError; }
        finally { if (rows is not null) await rows.DisposeAsync().ConfigureAwait(false); }

        if (expectedError is not null) Equal(expectedError, failure?.SqlState ?? "success", "SQLSTATE");
        else if (failure is not null) throw new InvalidDataException($"unexpected SQLSTATE {failure.SqlState}");
        if (record.Directives.TryGetValue("cost", out var cost))
            Equal(long.Parse(cost, CultureInfo.InvariantCulture), rows?.Cost ?? failure?.Cost ?? 0, "cost");
        if (query && failure is null)
        {
            var actual = cells.ToArray(); var expected = record.Expected.ToArray();
            switch (fields[2])
            {
                case "nosort": break;
                case "valuesort": Array.Sort(actual, StringComparer.Ordinal); Array.Sort(expected, StringComparer.Ordinal); break;
                case "rowsort":
                    actual = SortRows(actual, fields[1].Length); expected = SortRows(expected, fields[1].Length); break;
                default: throw new InvalidDataException("unsupported sort mode");
            }
            EqualList(expected, actual, "values");
        }
    }
    private static string[] SortRows(string[] values, int width)
    {
        if (width == 0 || values.Length % width != 0) throw new InvalidDataException("incomplete row");
        var rows = values.Chunk(width).ToArray();
        Array.Sort(rows, (a, b) =>
        {
            for (var i = 0; i < width; i++) { var order = string.CompareOrdinal(a[i], b[i]); if (order != 0) return order; }
            return 0;
        });
        return rows.SelectMany(row => row).ToArray();
    }
    private static string[] Split(string values) => values.Split(',').Select(s => s.Trim()).ToArray();
    internal static void Equal<T>(T expected, T actual, string what)
    {
        if (!EqualityComparer<T>.Default.Equals(expected, actual)) throw new InvalidDataException($"{what}: expected {expected}, got {actual}");
    }
    internal static void EqualList(string[] expected, string[] actual, string what)
    {
        if (!expected.SequenceEqual(actual)) throw new InvalidDataException($"{what}: expected [{string.Join(", ", expected)}], got [{string.Join(", ", actual)}]");
    }
}
