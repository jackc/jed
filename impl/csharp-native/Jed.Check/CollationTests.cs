using System.Text.Json;
using Jed;

internal static class CollationTests
{
    internal static async Task Host(string root)
    {
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(root, "impl/csharp-native/baseline.json")));
        var revision = baseline.RootElement.GetProperty("semantic_revision").GetString();
        var unicode = UnicodeData.Load(File.ReadAllBytes(Path.Combine(root, $"impl/csharp-native/.build/baseline-{revision}/spec/collation/fixtures/unicode.jucd")));
        await using var database = await Database.CreateInMemoryAsync();
        await using var session = database.CreateSession(new SessionOptions { UnicodeData = unicode });
        async Task<string> Query(string sql)
        {
            await using var rows = await session.QueryAsync(sql); var values = new List<string>();
            await foreach (var row in rows) values.Add(row[0].ToString()); return string.Join(',', values);
        }
        await session.SetDefaultCollationAsync("unicode");
        await Query("CREATE TABLE a(s text)");
        await session.SetDefaultCollationAsync("C");
        await Query("CREATE TABLE b(s text)");
        await Query("INSERT INTO a VALUES ('Z'),('a')"); await Query("INSERT INTO b VALUES ('Z'),('a')");
        Corpus.Equal("a,Z", await Query("SELECT s FROM a ORDER BY s"), "existing column freezes Unicode default");
        Corpus.Equal("Z,a", await Query("SELECT s FROM b ORDER BY s"), "new column uses changed C default");
        await using var reopenedSession = database.CreateSession(new SessionOptions { UnicodeData = unicode });
        Corpus.Equal("C", await reopenedSession.DefaultCollationAsync(), "default collation persisted across sessions");
        await using var pin = await database.PinAsync();
        Corpus.Equal("unicode", pin.Snapshot.Catalog.Tables.Single(table => table.Name == "a").Columns[0].Collation, "catalog freezes explicit default identity");
        Corpus.Equal<string?>(null, pin.Snapshot.Catalog.Tables.Single(table => table.Name == "b").Columns[0].Collation, "catalog stores C default canonically");
        Console.WriteLine("CollationTests: host default persistence and column freeze PASS");
    }
    internal static void Run(string root)
    {
        var package = Path.Combine(root, "impl/csharp-native");
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, "baseline.json")));
        var revision = baseline.RootElement.GetProperty("semantic_revision").GetString()!;
        var spec = Path.Combine(package, ".build/baseline-" + revision, "spec");
        using var vectors = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, ".build/collation-vectors.json")));
        Corpus.Equal(revision, vectors.RootElement.GetProperty("semantic_revision").GetString(), "collation vector baseline");
        var tables = new Dictionary<string, Collation>(StringComparer.Ordinal); var expectedTables = new Dictionary<string, string>(StringComparer.Ordinal); var artifacts = 0; var bundles = 0; var keys = 0;
        var files = vectors.RootElement.GetProperty("files").EnumerateArray().ToArray();
        foreach (var test in files.Single(file => file.GetProperty("source").GetString() == "collation/vectors/compiler.toml").GetProperty("data").GetProperty("compiler").EnumerateArray())
        {
            var bytes = Convert.FromHexString(test.GetProperty("artifact_hex").GetString()!); var table = Collation.Open(bytes);
            Corpus.Equal(test.GetProperty("table_hex").GetString(), Convert.ToHexStringLower(table.TableBytes()), "shared JCOLL compiled table bytes");
            Corpus.Equal(Convert.ToHexString(bytes), Convert.ToHexString(table.Save()), "JCOLL exact round trip");
            tables.Add(table.Name, table); expectedTables.Add(table.Name, test.GetProperty("table_hex").GetString()!); artifacts++;
        }
        foreach (var test in files.Single(file => file.GetProperty("source").GetString() == "collation/vectors/bundle.toml").GetProperty("data").GetProperty("bundle").EnumerateArray())
        {
            var bytes = Convert.FromHexString(test.GetProperty("bundle_hex").GetString()!); var data = UnicodeData.Load(bytes);
            Corpus.Equal(Convert.ToHexString(bytes), Convert.ToHexString(data.Save()), "JUCD exact round trip");
            foreach (var name in data.CollationNames) Corpus.Equal(expectedTables[name], Convert.ToHexStringLower(data.Resolve(name).TableBytes()), "JUCD root/tailoring merge identity");
            bundles++;
        }
        var production = UnicodeData.Load(File.ReadAllBytes(Path.Combine(spec, "collation/fixtures/unicode.jucd")));
        foreach (var name in production.CollationNames)
        {
            var artifact = Collation.Open(File.ReadAllBytes(Path.Combine(spec, "collation/fixtures/" + name + ".coll")));
            Corpus.Equal(Convert.ToHexString(artifact.TableBytes()), Convert.ToHexString(production.Resolve(name).TableBytes()), "production JUCD merge identity");
            tables.Add(name, production.Resolve(name));
        }
        foreach (var test in files.Single(file => file.GetProperty("source").GetString() == "collation/vectors/sortkey.toml").GetProperty("data").GetProperty("sortkey").EnumerateArray())
        {
            var table = tables[test.GetProperty("coll_name").GetString()!]; var text = test.GetProperty("string").GetString()!;
            Corpus.Equal(test.GetProperty("sortkey_hex").GetString(), Convert.ToHexStringLower(table.SortKey(text)), "shared UCA sort-key bytes"); keys++;
        }
        Corpus.Equal("CAFé", UnicodeData.Fold("café", true, null), "bare core ASCII casing");
        Corpus.Equal("STRASSE", UnicodeData.Fold("straße", true, production), "full Unicode expanding uppercase");
        Corpus.Equal("ß", UnicodeData.Fold("ß", false, production, false), "ILIKE simple casing preserves width");
        Console.WriteLine($"CollationTests: {artifacts} shared artifact read/roundtrips, {bundles} shared bundle merges, {keys} shared sort-key vectors (compiler tooling not claimed)");
    }
}
