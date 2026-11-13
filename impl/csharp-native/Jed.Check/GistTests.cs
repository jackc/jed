using Jed;

internal static class GistTests
{
    internal static async Task Run(string root)
    {
        var indexes = 0; var rows = 0;
        foreach (var name in new[] { "gist_range_table", "gist_scalar_table", "gist_exclude_table", "exclude_followon_table" })
        {
            var image = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures", name + ".jed"));
            await using var database = await Database.DeserializeAsync(image);
            using var pin = database.Pin();
            foreach (var table in pin.Snapshot.Catalog.Tables)
                foreach (var index in (table.Indexes ?? []).Where(index => index.Kind == 2))
                {
                    var persisted = index.Gist ?? throw new InvalidDataException("GiST was not loaded");
                    var types = index.Keys.Select(key => table.Columns[key.Column].Type.Type).ToArray();
                    var loaded = new List<StorageRecord>();
                    await foreach (var entry in persisted.Leaves.ScanAsync()) loaded.Add(entry);
                    // Recreate the canonical leaf set in reverse order and rebuild.
                    // This proves content determinism against the shared persisted tree.
                    var leaves = (await persisted.Leaves.ClearAsync()).CommittedResidentCopy();
                    foreach (var entry in loaded.AsEnumerable().Reverse()) leaves = await leaves.UpsertAsync(entry);
                    var rebuilt = await GistTree.BuildAsync(leaves, types, CancellationToken.None);
                    EqualNodes(persisted.Root, rebuilt.Root, name);
                    Corpus.Equal(0L, persisted.Leaves.PendingBytes, name + " loaded leaves are committed");
                    var primary = database.Tree(table, null, pin.Snapshot);
                    var session = database.CreateSession();
                    var meter = new Meter(session, 0, CancellationToken.None);
                    var expected = new List<string>();
                    await foreach (var record in primary.ScanAsync())
                    {
                        var values = table.DecodeRow(await primary.MaterializeRecordAsync(record));
                        if (StatementExecution.ResolveIndexPredicate(table, index, pin.Snapshot.Catalog) is { } predicate && !QueryExecution.Keeps(predicate, values, meter)) continue;
                        var bound = GistBounds.FromValues(index.Keys.Select(key => values[key.Column]).ToArray());
                        if (bound is not null) expected.Add(Convert.ToHexString(GistTree.LeafKey(bound, record.Key)));
                        rows++;
                    }
                    Corpus.Equal(string.Join(',', expected.Order(StringComparer.Ordinal)), string.Join(',', loaded.Select(row => Convert.ToHexString(row.Key)).Order(StringComparer.Ordinal)), name + " full GiST leaf set");
                    await session.DisposeAsync(); indexes++;
                }
        }
        Console.WriteLine($"PASS GiST: {indexes} indexes in four shared goldens, {rows} source rows, exact leaf keys/bounds and canonical tree structure");
    }
    private static void EqualNodes(GistNode? expected, GistNode? actual, string name)
    {
        Corpus.Equal(expected is null, actual is null, name + " empty tree");
        if (expected is null || actual is null) return;
        Corpus.Equal(expected.Leaf, actual.Leaf, name + " node kind");
        Corpus.Equal(expected.Entries.Length, actual.Entries.Length, name + " node entry count");
        for (var i = 0; i < expected.Entries.Length; i++)
        {
            Corpus.Equal(Convert.ToHexString(expected.Entries[i].Bound.Encode()), Convert.ToHexString(actual.Entries[i].Bound.Encode()), name + " exact bound bytes");
            Corpus.Equal(Convert.ToHexString(expected.Entries[i].Key), Convert.ToHexString(actual.Entries[i].Key), name + " exact row key");
            EqualNodes(expected.Entries[i].Child, actual.Entries[i].Child, name);
        }
    }
}
