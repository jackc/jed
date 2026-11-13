using Jed;

internal static class StatisticsTests
{
    internal static async Task Run(string root)
    {
        var summaries = 0;
        foreach (var name in new[] { "statistics_table", "statistics_decimal_table", "statistics_terms_table" })
        {
            var image = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures", name + ".jed"));
            await using var database = await Database.DeserializeAsync(image);
            await using var session = database.CreateSession();
            using var pin = database.Pin();
            var catalog = pin.Snapshot.Catalog;
            foreach (var summary in catalog.Statistics!.Where(stat => stat.Kind == 0))
            {
                var table = catalog.Tables.Single(table => table.Name == summary.Table);
                var tree = database.Tree(table, null, pin.Snapshot);
                var collector = new ColumnStatistics(table, summary.Column, new Meter(session, 0, CancellationToken.None));
                long count = 0;
                // The shared stale fixture appends one row after collecting its first
                // three storage-ordered rows; no fixture values are duplicated here.
                await foreach (var record in tree.ScanAsync())
                {
                    if (count++ == summary.AnalyzedRows) break;
                    collector.Add(record.Key, await tree.DecodeDeferredRow(table, record)[summary.Column].ResolveAsync(CancellationToken.None));
                }
                var actual = collector.Finish();
                actual[0] = actual[0] with { Flags = summary.Flags };
                var expected = catalog.Statistics!.Where(stat => stat.Table == summary.Table && stat.Column == summary.Column).OrderBy(stat => stat.Kind).ThenBy(stat => stat.ItemOrdinal).ToArray();
                Corpus.Equal(expected.Length, actual.Length, name + " statistic item count");
                for (var i = 0; i < expected.Length; i++)
                {
                    Corpus.Equal(expected[i] with { Value = null }, actual[i] with { Value = null }, name + " statistic fields");
                    Corpus.Equal(true, expected[i].Value.AsSpan().SequenceEqual(actual[i].Value), name + " statistic typed bytes");
                }
                summaries++;
            }
        }
        Console.WriteLine($"PASS statistics collection: {summaries} complete column facts from three shared byte fixtures, including fixed decimal and GIN term frequencies");
    }
}
