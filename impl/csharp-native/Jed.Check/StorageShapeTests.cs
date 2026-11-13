using Jed;

internal static class StorageShapeTests
{
    internal static async Task Run(string root)
    {
        var image = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/tall_tree.jed"));
        await using var store = new ReadCounter(image);
        await using var database = await Database.OpenStoreAsync(store);
        using var old = database.Pin(); var table = old.Snapshot.Catalog.Tables[0];
        var tree = database.Tree(table, null, old.Snapshot);
        StorageRecord? first = null;
        await foreach (var row in tree.ScanAsync()) { first = row; break; }
        if (first is null) throw new InvalidDataException("shape fixture is empty");
        var height = tree.Height; var count = tree.NodeCount;
        Corpus.Equal(true, height > 1 && count > height, "shape fixture has multiple interior/leaf branches");
        await Check(database, old.Snapshot, store, first.Key, height, count);
        var pending = await database.BeginWriteAsync(default);
        try
        {
            var dirty = await tree.UpsertAsync(first);
            Corpus.Equal(height, dirty.Height, "COW path retains height");
            Corpus.Equal(count, dirty.NodeCount, "COW path retains unchanged sibling structure");
            var reads = store.Reads;
            Corpus.Equal((long)height, dirty.CountRangeNodes(first.Key, true, first.Key, true), "dirty path point-bound count");
            Corpus.Equal(reads, store.Reads, "dirty shape planning performs no device reads");
            pending.Trees[table.Name] = dirty; pending.Changed = true;
            await database.CommitAsync(pending, default);
        }
        finally { await database.EndWriteAsync(pending); }
        using var current = database.Pin();
        await Check(database, current.Snapshot, store, first.Key, height, count);
        Corpus.Equal(count, tree.NodeCount, "old retained snapshot shape stays immutable");
        var exported = await database.SerializeAsync();
        await using var reopenedStore = new ReadCounter(exported);
        await using var reopened = await Database.OpenStoreAsync(reopenedStore);
        using var reopenedPin = reopened.Pin();
        await Check(reopened, reopenedPin.Snapshot, reopenedStore, first.Key, height, count);
        Console.WriteLine("PASS resident tree shapes: zero-read planning after eviction, dirty paths, commit and reopen preserve exact height/node/range counts");
    }
    private static async Task Check(Database database, DatabaseSnapshot snapshot, ReadCounter store, byte[] key, int height, long count)
    {
        for (uint page = 2; page < snapshot.Meta.PageCount; page++) snapshot.Pager!.Invalidate(page);
        Corpus.Equal(0, snapshot.Pager!.ResidentPages, "shape test evicts every page buffer");
        var reads = store.Reads;
        var tree = database.Tree(snapshot.Catalog.Tables[0], null, snapshot);
        Corpus.Equal(height, tree.Height, "retained tree height");
        Corpus.Equal(count, tree.NodeCount, "retained tree node count");
        Corpus.Equal(count, tree.CountRangeNodes(null, true, null, true), "full-range count is resident");
        Corpus.Equal((long)height, await tree.CountRangePagesAsync(key, true, key, true), "point-bound path count is resident");
        Corpus.Equal(reads, store.Reads, "shape planning never reads evicted pages");
    }
    private sealed class ReadCounter(byte[] image) : IBlockStore
    {
        private readonly MemoryBlockStore inner = new(image);
        internal int Reads;
        public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default) { Reads++; return inner.ReadAsync(offset, destination, token); }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default) => inner.WriteAsync(offset, source, token);
        public ValueTask<long> GetLengthAsync(CancellationToken token = default) => inner.GetLengthAsync(token);
        public ValueTask SetLengthAsync(long length, CancellationToken token = default) => inner.SetLengthAsync(length, token);
        public ValueTask SyncAsync(CancellationToken token = default) => inner.SyncAsync(token);
        public ValueTask DisposeAsync() => inner.DisposeAsync();
    }
}
