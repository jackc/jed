using System.Text.Json;
using Jed;

internal static class TemporalTests
{
    internal static void Run(string root)
    {
        var package = Path.Combine(root, "impl/csharp-native");
        using var document = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, ".build/temporal-vectors.json")));
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, "baseline.json")));
        Corpus.Equal(baseline.RootElement.GetProperty("semantic_revision").GetString(), document.RootElement.GetProperty("semantic_revision").GetString(), "temporal fixture baseline");
        var count = 0; var files = 0;
        foreach (var file in document.RootElement.GetProperty("files").EnumerateArray())
        {
            var source = file.GetProperty("source").GetString()!;
            var data = file.GetProperty("data");
            var isInterval = source == "encoding/intervals.toml";
            foreach (var mode in new[] { "parse", "parse_error", "render" })
            foreach (var group in data.GetProperty(mode).EnumerateArray())
            {
                var type = isInterval ? SqlType.Interval : SqlTypes.Parse(group.GetProperty("type").GetString()!);
                foreach (var item in group.GetProperty("cases").EnumerateArray())
                {
                    var label = $"{source} {mode} {item}";
                    if (mode == "parse_error")
                    {
                        try { _ = Parse(type, item.GetProperty("input").GetString()!); throw new InvalidDataException($"expected temporal error: {label}"); }
                        catch (SqlException error) { Corpus.Equal(item.GetProperty("error").GetString(), error.SqlState, label); }
                    }
                    else
                    {
                        var value = Raw(type, item);
                        if (mode == "render") Corpus.Equal(item.GetProperty("text").GetString(), value.ToString(), label);
                        else
                        {
                            var parsed = Parse(type, item.GetProperty("input").GetString()!);
                            // Field equality matters for intervals: span equality alone would conceal parser bugs.
                            Corpus.Equal(Convert.ToHexString(StoredValues.Encode(value)), Convert.ToHexString(StoredValues.Encode(parsed)), label);
                        }
                        var bytes = StoredValues.Encode(value);
                        var reader = new WireReader(bytes);
                        var restored = StoredValues.Decode(ref reader, type); reader.Finish();
                        Corpus.Equal(value.ToString(), restored.ToString(), "temporal stored representation");
                    }
                    count++;
                }
            }
            files++;
        }
        Corpus.Equal(3, files, "all complete temporal fixture files");
        var clockReads = 0;
        var context = new TemporalContext(() => ++clockReads, "UTC");
        Corpus.Equal(0, clockReads, "temporal context leaves host clock untouched until needed");
        Corpus.Equal(1L, context.StatementMicros, "first stable clock read");
        Corpus.Equal(1L, context.StatementMicros, "stable clock is cached");
        Corpus.Equal(2L, context.ClockMicros(), "volatile clock is fresh");
        Corpus.Equal(1L, context.StatementMicros, "volatile read does not replace statement clock");
        Corpus.Equal(3L, new TemporalContext(() => ++clockReads, "UTC").StatementMicros, "statement clock resets at next statement");
        var monthValue = SqlValue.Interval(new(1, 0, 0));
        var dayValue = SqlValue.Interval(new(0, 30, 0));
        Corpus.Equal(Convert.ToHexString(KeyEncoding.Encode(monthValue)), Convert.ToHexString(KeyEncoding.Encode(dayValue)), "span equal intervals share a key");
        if (StoredValues.Encode(monthValue).AsSpan().SequenceEqual(StoredValues.Encode(dayValue))) throw new InvalidDataException("interval value codec erased representation");
        // These inverse/invariant checks cover the primitive boundaries outside the SQL corpus.
        foreach (var days in new[] { int.MinValue + 1, -719528, -1, 0, 19782, int.MaxValue - 1 })
        {
            var civil = TemporalValue.CivilFromDays(days);
            Corpus.Equal((long)days, TemporalValue.DaysFromCivil(civil.Year, civil.Month, civil.Day), "wide Gregorian inverse");
            Corpus.Equal(days, TemporalInput.ParseDate(TemporalInput.RenderDate(days)), "wide date text inverse");
        }
        foreach (var micros in new[] { long.MinValue + 1, -1L, 0, 1, long.MaxValue - 1 })
            Corpus.Equal(micros, TemporalInput.ParseTimestamp(TemporalInput.RenderTimestamp(micros)), "wide timestamp text inverse");
        var random = new Random(914);
        for (var i = 0; i < 4000; i++)
        {
            var days = random.Next(int.MinValue + 1, int.MaxValue);
            var civil = TemporalValue.CivilFromDays(days);
            Corpus.Equal((long)days, TemporalValue.DaysFromCivil(civil.Year, civil.Month, civil.Day), "Gregorian inverse");
            var interval = new SqlInterval(random.Next(-10000, 10001), random.Next(-10000, 10001), random.NextInt64());
            Corpus.Equal(interval.Span, interval.Justify().Span, "interval justify span invariant");
            Corpus.Equal(interval.Span, interval.JustifyDays().Span, "interval justify days invariant");
            Corpus.Equal(interval.Span, interval.JustifyHours().Span, "interval justify hours invariant");
        }
        Console.WriteLine($"PASS temporal: {files} complete shared fixture files, {count} parse/error/render vectors; wide calendar inverses, interval span invariants and stored representation round trips");
    }
    private static SqlValue Parse(SqlType type, string text) => type.Kind switch
    {
        SqlKind.Date => SqlValue.Date(TemporalInput.ParseDate(text)),
        SqlKind.Timestamp or SqlKind.Timestamptz => SqlValue.Timestamp(TemporalInput.ParseTimestamp(text, type == SqlType.Timestamptz), type == SqlType.Timestamptz),
        SqlKind.Interval => SqlValue.Interval(IntervalInput.Parse(text)),
        _ => throw new InvalidDataException("unknown temporal fixture type")
    };
    private static SqlValue Raw(SqlType type, JsonElement item) => type.Kind switch
    {
        SqlKind.Date => SqlValue.Date(item.GetProperty("days").GetInt32()),
        SqlKind.Timestamp or SqlKind.Timestamptz => SqlValue.Timestamp(item.GetProperty("micros").GetInt64(), type == SqlType.Timestamptz),
        SqlKind.Interval => SqlValue.Interval(new(item.GetProperty("months").GetInt32(), item.GetProperty("days").GetInt32(), item.GetProperty("micros").GetInt64())),
        _ => throw new InvalidDataException("unknown temporal fixture type")
    };
}
