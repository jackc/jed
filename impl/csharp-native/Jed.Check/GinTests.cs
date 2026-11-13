using System.Text.Json;
using Jed;

internal static class GinTests
{
    private static byte[] Bytes(JsonElement element) => Convert.FromHexString(element.GetString()!);
    private static byte[][] BytesList(JsonElement element) => element.EnumerateArray().Select(Bytes).ToArray();
    private static void Equal(IEnumerable<byte[]> expected, IEnumerable<byte[]> actual, string label) => Corpus.Equal(
        string.Join(',', expected.Select(Convert.ToHexString)), string.Join(',', actual.Select(Convert.ToHexString)), label);
    internal static async Task Run(string root)
    {
        var package = Path.Combine(root, "impl/csharp-native");
        using var baseline = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, "baseline.json")));
        using var vectors = JsonDocument.Parse(File.ReadAllBytes(Path.Combine(package, ".build/gin-vectors.json")));
        Corpus.Equal(baseline.RootElement.GetProperty("semantic_revision").GetString(), vectors.RootElement.GetProperty("semantic_revision").GetString(), "GIN fixture baseline");
        var terms = 0; var codec = 0; var packs = 0; var histories = 0;
        foreach (var file in vectors.RootElement.GetProperty("files").EnumerateArray())
        {
            var data = file.GetProperty("data");
            if (data.TryGetProperty("index", out var documents)) foreach (var test in documents.EnumerateArray())
            {
                Equal(BytesList(test.GetProperty("terms")), StatisticsTerms.Extract(Resolve.ParseText(test.GetProperty("doc").GetString()!, SqlType.Jsonb)), "GIN jsonb term set"); terms++;
            }
            if (data.TryGetProperty("key", out var keys)) foreach (var test in keys.EnumerateArray())
            {
                Equal([Bytes(test.GetProperty("term"))], StatisticsTerms.Extract(SqlValue.Jsonb(SqlJsonNode.String(test.GetProperty("key").GetString()!))), "GIN key term"); terms++;
            }
            if (data.TryGetProperty("run", out var runs)) foreach (var test in runs.EnumerateArray())
            {
                var term = Bytes(test.GetProperty("term")); var input = BytesList(test.GetProperty("keys")); var encoded = GinRuns.Encode(term, input);
                Equal([Bytes(test.GetProperty("run"))], [encoded], "GIN run encoding"); Equal(input, GinRuns.Decode(encoded, term.Length), "GIN run decoding"); codec++;
            }
            if (data.TryGetProperty("pack", out var packing)) foreach (var test in packing.EnumerateArray())
            { Equal(BytesList(test.GetProperty("runs")), GinRuns.Pack(Bytes(test.GetProperty("term")), BytesList(test.GetProperty("keys"))), "GIN greedy packing"); packs++; }
            if (data.TryGetProperty("history", out var history)) foreach (var test in history.EnumerateArray())
            {
                await using var device = new MemoryBlockStore(); var tree = new StorageTree(new StoragePager(device, 256), new([]));
                var term = Bytes(test.GetProperty("term"));
                foreach (var operation in test.GetProperty("ops").EnumerateArray())
                { var text = operation.GetString()!; tree = await GinRuns.Edit(tree, term, Convert.FromHexString(text[1..]), text[0] == '+', CancellationToken.None); }
                var actual = new List<byte[]>(); await foreach (var record in tree.ScanAsync()) actual.Add(record.Key);
                Equal(BytesList(test.GetProperty("runs")), actual, "GIN maintenance " + test.GetProperty("name").GetString()); histories++;
            }
        }
        var images = 0;
        foreach (var name in new[] { "gin_array_table", "gin_multi_table", "gin_jsonb_table", "gin_float_table", "gin_interval_table", "gin_uuid_table", "gin_text_table", "gin_decimal_bytea_table" })
        {
            var expected = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures", name + ".jed"));
            await using var database = await Database.DeserializeAsync(expected);
            await using var session = database.CreateSession();
            using var before = database.Pin();
            static string Name(string value) => "\"" + value.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";
            foreach (var table in before.Snapshot.Catalog.Tables)
                foreach (var index in table.Indexes ?? [])
                {
                    if (index.Kind != 1) continue;
                    await DatabaseFileTests.Exec(session, "DROP INDEX " + Name(index.Name));
                    await DatabaseFileTests.Exec(session, "CREATE INDEX " + Name(index.Name) + " ON " + Name(table.Name) + " USING gin (" + string.Join(',', index.Keys.Select(key => Name(table.Columns[key.Column].Name))) + ")");
                }
            var actual = await database.SerializeAsync();
            using var after = database.Pin();
            // DROP/CREATE advances only the fixture's transaction history. Every
            // structural meta field and every catalog/data/index byte still match.
            var meta = StorageFormat.EncodeMeta(before.Snapshot.Meta with { TransactionId = after.Snapshot.Meta.TransactionId });
            meta.CopyTo(expected, 0); meta.CopyTo(expected, before.Snapshot.Meta.PageSize);
            if (!expected.AsSpan().SequenceEqual(actual))
            {
                await File.WriteAllBytesAsync(Path.Combine(Path.GetTempPath(), "jed-csharp-" + name + "-actual.jed"), actual);
                throw new InvalidDataException("GIN rebuilt complete image " + name + " differs; actual bytes saved under temporary directory");
            }
            images++;
        }
        Console.WriteLine($"PASS GIN storage: {images} complete shared images rebuilt natively from stored rows");
        Console.WriteLine($"PASS GIN byte vectors: {terms} complete JSON term cases, {codec} run codecs, {packs} greedy builds, {histories} maintenance histories");
    }
}
