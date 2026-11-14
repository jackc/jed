using Jed;

internal static class StorageScanTests
{
    internal static async Task Run()
    {
        // Three levels force traversal across sibling subtrees, including
        // empty ranges. Test bounds against a simple ordered-key oracle.
        const int pageSize = 256;
        var schema = new StorageLeafSchema([new(Width: 4, KeyOffset: 0, Integer: true)], FixedKeyWidth: 4);
        byte[] Key(int value) => KeyEncoding.Integer(value, 4);
        StorageRecord Record(int value) => new(Key(value), [Key(value)]);
        await using var device = new MemoryBlockStore();
        await device.SetLengthAsync(10 * pageSize);
        var leaves = new[] { new[] { 1, 3, 5 }, new[] { 7 }, new[] { 9, 11 }, new[] { 13, 15, 17 } };
        for (var i = 0; i < leaves.Length; i++)
            await device.WriteAsync((i + 2) * pageSize, StorageLeaf.Encode(pageSize, schema, leaves[i].Select(Record).ToArray()));
        await device.WriteAsync(6 * pageSize, new StorageInterior([2, 3], [Key(7)]).Encode(pageSize));
        await device.WriteAsync(7 * pageSize, new StorageInterior([4, 5], [Key(13)]).Encode(pageSize));
        await device.WriteAsync(8 * pageSize, new StorageInterior([6, 7], [Key(9)]).Encode(pageSize));
        var pager = new StoragePager(device, pageSize, cacheBytes: pageSize);
        var tree = new StorageTree(pager, schema, 8, 10);
        var all = leaves.SelectMany(values => values).ToArray();
        var table = new CatalogTable("t", [new CatalogColumn("id", new(SqlType.I32))], [0]);
        var destination = new SqlValue[1];
        foreach (int? lower in new int?[] { null, 0, 3, 7, 10, 17, 18 })
        foreach (int? upper in new int?[] { null, 0, 3, 7, 10, 17, 18 })
        foreach (var lowerInclusive in new[] { false, true })
        foreach (var upperInclusive in new[] { false, true })
        foreach (var reverse in new[] { false, true })
        {
            var expected = all.Where(value => (lower is null || (lowerInclusive ? value >= lower : value > lower)) &&
                (upper is null || (upperInclusive ? value <= upper : value < upper)));
            if (reverse) expected = expected.Reverse();
            var actual = new List<long>();
            await foreach (var row in tree.ScanRangeAsync(lower is null ? null : Key(lower.Value), lowerInclusive,
                upper is null ? null : Key(upper.Value), upperInclusive, reverse))
                actual.Add(KeyEncoding.ReadInteger(row.Key));
            Corpus.Equal(string.Join(",", expected), string.Join(",", actual), "multilevel scan bounds and order");
            var chunked = new List<long>();
            await foreach (var chunk in tree.ScanRangeChunksAsync(lower is null ? null : Key(lower.Value), lowerInclusive,
                upper is null ? null : Key(upper.Value), upperInclusive, reverse))
            {
                Corpus.Equal(reverse, chunk.Reverse, "chunk direction");
                for (var i = reverse ? chunk.Until - 1 : chunk.From; reverse ? i >= chunk.From : i < chunk.Until; i += reverse ? -1 : 1)
                {
                    chunk.DecodeFixedInto(table, tree, i, destination);
                    Corpus.Equal(destination[0].AsInteger(), chunk.Row(i).DecodeFixed(table, tree)[0].AsInteger(), "chunk reusable decode equals owned row");
                    chunked.Add(destination[0].AsInteger());
                }
            }
            Corpus.Equal(string.Join(",", actual), string.Join(",", chunked), "chunk and ordinary rows agree across all bounds");
        }
        var resident = new StorageTree(pager, schema, 8, 10, residentRoot: all.Select(Record).ToArray());
        await foreach (var chunk in resident.ScanRangeChunksAsync(Key(3), true, Key(15), false))
        {
            var saved = chunk.Row(chunk.From).DecodeFixed(table, resident);
            for (var i = chunk.From; i < chunk.Until; i++)
            {
                chunk.DecodeFixedInto(table, resident, i, destination);
                Corpus.Equal(KeyEncoding.ReadInteger(chunk.Row(i).Record().Key), destination[0].AsInteger(), "resident chunk reusable decode");
            }
            Corpus.Equal(3L, saved[0].AsInteger(), "owned row survives reuse of internal destination");
        }
        await AccessRows(table, tree);
        await AccessRows(table, resident);
        var observed = new ObservedStore(device);
        var asynchronous = new StorageTree(new StoragePager(observed, pageSize, cacheBytes: pageSize), schema, 8, 10);
        using (var chunkCancellation = new CancellationTokenSource())
        await using (var iterator = asynchronous.ScanRangeChunksAsync(null, true, null, true, cancellationToken: chunkCancellation.Token).GetAsyncEnumerator())
        {
            Corpus.Equal(0, observed.Reads, "chunk construction performs no IO");
            Corpus.Equal(true, await iterator.MoveNextAsync(), "first asynchronously loaded chunk");
            var held = iterator.Current;
            Corpus.Equal(3, observed.Reads, "first chunk loads only root path and first leaf");
            Corpus.Equal(true, await iterator.MoveNextAsync(), "next chunk across awaited page read");
            Corpus.Equal(4, observed.Reads, "next chunk loads only next leaf");
            held.DecodeFixedInto(table, asynchronous, held.From, destination);
            Corpus.Equal(1L, destination[0].AsInteger(), "held chunk survives cache eviction");
            chunkCancellation.Cancel();
            var cancelled = false;
            try { _ = await iterator.MoveNextAsync(); } catch (OperationCanceledException) { cancelled = true; }
            Corpus.Equal(true, cancelled, "chunk cancellation before next leaf");
            Corpus.Equal(4, observed.Reads, "cancelled chunk performs no speculative reads");
        }
        using var cancellation = new CancellationTokenSource();
        await using (var iterator = tree.ScanRangeRowsAsync(null, true, null, true, cancellationToken: cancellation.Token).GetAsyncEnumerator())
        {
            Corpus.Equal(true, await iterator.MoveNextAsync(), "scan first row before cancellation");
            cancellation.Cancel();
            var cancelled = false;
            try { _ = await iterator.MoveNextAsync(); } catch (OperationCanceledException) { cancelled = true; }
            Corpus.Equal(true, cancelled, "resident scan checks cancellation between rows");
        }
        // A back edge must remain detectable with the iterative traversal.
        await device.WriteAsync(9 * pageSize, new StorageInterior([9], []).Encode(pageSize));
        var cycle = new StorageTree(pager, schema, 9, 10);
        string? error = null;
        try { await foreach (var _ in cycle.ScanRangeChunksAsync(null, true, null, true)) { } }
        catch (SqlException failure) { error = failure.SqlState; }
        Corpus.Equal("XX001", error, "scan detects cyclic interior");
        Console.WriteLine("PASS iterative storage scan: row/chunk equivalence, owned/reused decoding, bounded async IO, bounds, cancellation and cycles");
    }
    private static async Task AccessRows(CatalogTable table, StorageTree tree)
    {
        await using var database = await Database.CreateInMemoryAsync();
        await using var pin = await database.PinAsync();
        var storage = new QueryStorage(database, pin.Snapshot, null, pin.Snapshot.Catalog);
        byte[] Key(int value) => KeyEncoding.Integer(value, 4);
        var plans = new AccessPlan[] {
            new(AccessKind.Full, 0, 0, 0, 0, Scan: new()),
            new(AccessKind.Primary, 0, 0, 0, 0, Scan: new(Key(3), false, Key(15), true)),
            new(AccessKind.Primary, 0, 0, 0, 0, Scan: new(Key(7), true, Key(7), true)),
            new(AccessKind.Primary, 0, 0, 0, 0, Scan: new(Key(8), true, Key(8), true)),
            new(AccessKind.Primary, 0, 0, 0, 0, Scan: new(Key(7), false, Key(7), true)),
            new(AccessKind.PrimaryIntervals, 0, 0, 0, 0, Ranges: [new(Key(3), false, Key(7), true), new(Key(13), true, Key(17), false)]),
            new(AccessKind.PrimaryIntervals, 0, 0, 0, 0, Ranges: [new(Key(3), false, Key(7), true), new(Key(13), true, Key(17), false)], Streaming: true)
        };
        foreach (var plan in plans)
        foreach (var reverse in new[] { false, true })
        foreach (var ceiling in new long[] { 0, 1, 3, 7, 9, 13 })
        {
            await using var actualSession = new Session();
            await using var referenceSession = new Session();
            var actualMeter = new Meter(actualSession, ceiling, default);
            var referenceMeter = new Meter(referenceSession, ceiling, default);
            var actual = new List<SqlValue[]>(); var expected = new List<long>();
            string? actualError = null, expectedError = null;
            try
            {
                await foreach (var row in plan.Rows(table, tree, storage, actualMeter, null, default, reverse)) actual.Add(row);
            }
            catch (SqlException failure) { actualError = failure.SqlState; }
            try
            {
                await foreach (var row in plan.Records(table, tree, storage, referenceMeter, null, default, reverse))
                    expected.Add(KeyEncoding.ReadInteger(row.Key));
            }
            catch (SqlException failure) { expectedError = failure.SqlState; }
            Corpus.Equal(string.Join(",", expected), string.Join(",", actual.Select(row => row[0].AsInteger())), "chunked access rows equal record traversal");
            Corpus.Equal(expectedError, actualError, "chunked access cost ceiling error and partial rows");
            Corpus.Equal(referenceMeter.Accrued, actualMeter.Accrued, "chunked access exact charge schedule");
            for (var i = 1; i < actual.Count; i++)
                Corpus.Equal(false, ReferenceEquals(actual[i - 1], actual[i]), "each access row owns its destination");
        }
        using var cancellation = new CancellationTokenSource();
        await using var session = new Session();
        var meter = new Meter(session, 0, cancellation.Token);
        await using var iterator = plans[0].Rows(table, tree, storage, meter, null, cancellation.Token).GetAsyncEnumerator();
        Corpus.Equal(true, await iterator.MoveNextAsync(), "chunked access first row");
        var accrued = meter.Accrued;
        cancellation.Cancel();
        var cancelled = false;
        try { _ = await iterator.MoveNextAsync(); } catch (OperationCanceledException) { cancelled = true; }
        Corpus.Equal(true, cancelled, "chunked access cancellation between rows in same leaf");
        Corpus.Equal(accrued, meter.Accrued, "chunked access cancellation precedes next row charge");
    }
    private sealed class ObservedStore(IBlockStore inner) : IBlockStore
    {
        internal int Reads;
        public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default)
        { Reads++; await Task.Yield(); await inner.ReadAsync(offset, destination, token); }
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default) => inner.WriteAsync(offset, source, token);
        public ValueTask<long> GetLengthAsync(CancellationToken token = default) => inner.GetLengthAsync(token);
        public ValueTask SetLengthAsync(long length, CancellationToken token = default) => inner.SetLengthAsync(length, token);
        public ValueTask SyncAsync(CancellationToken token = default) => inner.SyncAsync(token);
        public ValueTask DisposeAsync() => ValueTask.CompletedTask;
    }
}
