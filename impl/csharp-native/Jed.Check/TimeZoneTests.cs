using System.Text.Json;
using Jed;

internal static class TimeZoneTests
{
    internal static void Run(string root)
    {
        var package = Path.Combine(root, "impl/csharp-native");
        using var document = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, ".build/timezone-vectors.json")));
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, "baseline.json")));
        var revision = baseline.RootElement.GetProperty("semantic_revision").GetString();
        Corpus.Equal(revision, document.RootElement.GetProperty("semantic_revision").GetString(), "timezone fixture baseline");
        var bytes = File.ReadAllBytes(Path.Combine(package, $".build/baseline-{revision}/spec/tz/fixtures/tzdata.jtz"));
        var data = TimeZoneData.Load(bytes); var count = 0;
        foreach (var file in document.RootElement.GetProperty("files").EnumerateArray())
        {
            var vectors = file.GetProperty("data"); Corpus.Equal(1, vectors.GetProperty("schema_version").GetInt32(), "timezone schema");
            if (vectors.TryGetProperty("case", out var cases))
                foreach (var item in cases.EnumerateArray())
                {
                    var zone = data.Resolve(item.GetProperty("zone").GetString()!);
                    var instant = item.GetProperty("instant_micros").GetInt64(); var offset = zone.OffsetAt(instant);
                    Corpus.Equal(item.GetProperty("utoff_secs").GetInt32(), offset.Seconds, "TZif offset");
                    Corpus.Equal(item.GetProperty("abbrev").GetString(), offset.Abbreviation, "TZif abbreviation");
                    Corpus.Equal(item.GetProperty("is_dst").GetBoolean(), offset.IsDaylight, "TZif DST flag"); count++;
                }
            else foreach (var item in vectors.GetProperty("bundle").EnumerateArray())
            {
                Corpus.Equal(item.GetProperty("tzdata_version").GetString(), data.Version, "JTZ version");
                Corpus.Equal(string.Join(',', item.GetProperty("zones").EnumerateArray().Select(v => v.GetString())), string.Join(',', data.ZoneNames), "JTZ zones");
                Corpus.Equal(string.Join(',', item.GetProperty("links").EnumerateArray().Select(v => v.GetString())), string.Join(',', data.Links.OrderBy(pair => pair.Key, StringComparer.Ordinal).Select(pair => pair.Key + "=" + pair.Value)), "JTZ aliases");
                Corpus.Equal(true, data.Save().AsSpan().SequenceEqual(bytes), "JTZ byte-exact save");
            }
        }
        var corrupt = bytes.ToArray(); corrupt[corrupt.Length / 2] ^= 1;
        try { _ = TimeZoneData.Load(corrupt); throw new InvalidDataException("corrupt JTZ accepted"); }
        catch (SqlException error) when (error.SqlState == SqlStates.InvalidParameterValue) { }
        Console.WriteLine($"PASS timezone bytes: complete shared TZif and JTZ vector files, {count} offset/abbreviation/DST cases, exact bundle save and checksum rejection");
    }
}
