using System.Globalization;
using System.Text;
using System.Text.Json;
using Jed;

internal sealed record SimulationRows(string[] Rendered, long Affected, long Cost)
{
    internal string Hash => Fold(string.Join('\u001e', Rendered));
    internal static async Task<SimulationRows> Query(Session session, string sql)
    {
        var rendered = new List<string>();
        var context = session.NewTemporalContext();
        await using var rows = await session.QueryAsync(sql).ConfigureAwait(false);
        await foreach (var row in rows)
        {
            var cells = new string[row.Count];
            for (var i = 0; i < cells.Length; i++) cells[i] = TemporalRender.Value(row[i], context);
            rendered.Add(string.Join('\u001f', cells));
        }
        rendered.Sort(Utf8NameComparer.Instance);
        return new(rendered.ToArray(), rows.AffectedRows ?? -1, rows.Cost);
    }
    internal static async Task<string> Digest(Session session, SimulationFileSystem fs, string name = "main")
    {
        var originalOracle = fs.OracleReads; fs.OracleReads = true;
        var limit = session.MaxQueryMemoryBytes;
        if (limit > 0) session.SetQueryMemoryLimit(0);
        try
        {
            var prefix = name == "main" ? "" : name + ".";
            var text = new StringBuilder(); var tables = Array.Empty<string>();
            foreach (var relation in new[] { "jed_tables", "jed_columns", "jed_indexes", "jed_constraints", "jed_sequences" })
            {
                var sql = "SELECT * FROM " + prefix + relation;
                var result = await Query(session, sql).ConfigureAwait(false);
                if (relation == "jed_tables") tables = result.Rendered.Select(row => row.Split('\u001f')[0]).Order(Utf8NameComparer.Instance).ToArray();
                text.Append(sql).Append('\n').AppendJoin('\n', result.Rendered).Append("\n\n");
            }
            foreach (var table in tables)
            {
                var sql = "SELECT * FROM " + prefix + "\"" + table.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";
                var result = await Query(session, sql).ConfigureAwait(false);
                text.Append(sql).Append('\n').AppendJoin('\n', result.Rendered).Append("\n\n");
            }
            return Fold(text.ToString());
        }
        finally { fs.OracleReads = originalOracle; if (limit > 0) session.SetQueryMemoryLimit(limit); }
    }
    internal static string Fold(string value) => Fold(Encoding.UTF8.GetBytes(value));
    internal static string Fold(ReadOnlySpan<byte> bytes)
    {
        ulong hash = 14695981039346656037;
        unchecked { foreach (var value in bytes) hash = (hash ^ value) * 1099511628211; }
        return hash.ToString("x16", CultureInfo.InvariantCulture);
    }
}

internal sealed class SimulationTranscript : IDisposable
{
    private readonly MemoryStream stream = new();
    private readonly Utf8JsonWriter writer;
    internal SimulationTranscript() => writer = new(stream);
    internal SimulationTranscript Start(int step, string operation)
    { writer.WriteStartObject(); writer.WriteNumber("i", step); writer.WriteString("op", operation); return this; }
    internal SimulationTranscript Text(string name, string value) { writer.WriteString(name, value); return this; }
    internal SimulationTranscript Number(string name, long value) { writer.WriteNumber(name, value); return this; }
    internal SimulationTranscript Number(string name, ulong value) { writer.WriteNumber(name, value); return this; }
    internal SimulationTranscript Flag(string name, bool value) { writer.WriteBoolean(name, value); return this; }
    internal SimulationTranscript Object(string name) { writer.WriteStartObject(name); return this; }
    internal SimulationTranscript EndObject() { writer.WriteEndObject(); return this; }
    internal string End()
    {
        writer.WriteEndObject(); writer.Flush();
        var result = Encoding.UTF8.GetString(stream.GetBuffer(), 0, checked((int)stream.Length));
        stream.SetLength(0); writer.Reset(stream); return result;
    }
    public void Dispose() { writer.Dispose(); stream.Dispose(); }
}
