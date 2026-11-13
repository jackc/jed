using Jed;

internal static class DatabaseDomainCommitTests
{
    internal static async Task Run(string root)
    {
        var image = await File.ReadAllBytesAsync(Path.Combine(root, "spec/fileformat/fixtures/empty_db.jed"));
        await using var firstStore = new StageStore(image);
        await using var laterStore = new StageStore(image);
        await using var main = await Database.OpenStoreAsync(firstStore, ephemeral: true);
        await using var later = await Database.OpenStoreAsync(laterStore, ephemeral: true);
        var first = await main.BeginWriteAsync(default);
        var second = await later.BeginWriteAsync(default);
        first.Changed = second.Changed = true;
        // The limited domain must stage a record version: a metadata-only rewrite
        // is deliberately admissible under the storage repair exemption.
        var growth = new CatalogTable("growth", [new("n", new(SqlType.I32), NotNull: true)], [0], RowCount: 1);
        second.Catalog = second.Catalog with { Tables = [growth] };
        second.Trees[growth.Name] = await later.Tree(growth, null, second.Before).UpsertAsync(growth.EncodeRow([SqlValue.Integer(1, SqlType.I32)]));
        var scope = new DatabaseDomains([new("main", main, first.Before, first), new("z", later, second.Before, second)]);
        try
        {
            later.MaxStorageBytes = 1;
            await HostTests.Error(SqlStates.StorageLimitExceeded, () => Database.CommitDomainsAsync(scope));
            Corpus.Equal(0, firstStore.Writes + laterStore.Writes, "all domain limits precede first staged write");
            later.MaxStorageBytes = 0; laterStore.Fail = true;
            try { await Database.CommitDomainsAsync(scope); throw new InvalidDataException("stage fault not reached"); }
            catch (IOException) { }
            Corpus.Equal(true, firstStore.Writes > 0, "earlier domain staged before later storage failure");
            using (var pin = main.Pin()) Corpus.Equal(first.Before.Meta, pin.Snapshot.Meta, "staged domain root/accounting remain unpublished");
            var exported = await main.SerializeAsync();
            Corpus.Equal(true, image.AsSpan().SequenceEqual(exported), "abandoned pages do not alter published image");
            laterStore.Fail = false;
            await Database.CommitDomainsAsync(scope);
            using (var pin = main.Pin()) Corpus.Equal(first.Before.Meta.TransactionId + 1, pin.Snapshot.Meta.TransactionId, "first staged domain publishes on retry");
            using (var pin = later.Pin()) Corpus.Equal(second.Before.Meta.TransactionId + 1, pin.Snapshot.Meta.TransactionId, "later staged domain publishes atomically");
            Corpus.Equal(0, firstStore.Syncs + laterStore.Syncs, "memory staging performs no durable barriers");
        }
        finally { await later.EndWriteAsync(second); await main.EndWriteAsync(first); }
        var directory = Path.Combine(Path.GetTempPath(), "jed-domain-commit-" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(directory);
        try
        {
            await using var a = await Database.CreateFileAsync(Path.Combine(directory, "a.jed"), pageSize: 256);
            await using var b = await Database.CreateFileAsync(Path.Combine(directory, "b.jed"), pageSize: 256);
            var pa = await a.BeginWriteAsync(default); var pb = await b.BeginWriteAsync(default);
            pa.Changed = pb.Changed = true;
            try
            {
                var domains = new DatabaseDomains([new("main", a, pa.Before, pa), new("attached", b, pb.Before, pb)]);
                await HostTests.Error(SqlStates.FeatureNotSupported, () => Database.CommitDomainsAsync(domains));
                await using var apin = await a.PinAsync(); await using var bpin = await b.PinAsync();
                Corpus.Equal(pa.Before.Meta, apin.Snapshot.Meta, "two durable writers reject before publication");
                Corpus.Equal(pb.Before.Meta, bpin.Snapshot.Meta, "second durable writer stays unchanged");
            }
            finally { await b.EndWriteAsync(pb); await a.EndWriteAsync(pa); }
        }
        finally { Directory.Delete(directory, true); }
        Console.WriteLine("PASS domain commit invariants: all-plan preflight, staged-memory failure/retry, unchanged published accounting, no memory barriers, two-file rejection");
    }
    private sealed class StageStore(byte[] bytes) : IBlockStore
    {
        private readonly MemoryBlockStore inner = new(bytes);
        internal int Writes;
        internal int Syncs;
        internal bool Fail;
        public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default) => inner.ReadAsync(offset, destination, token);
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default)
        { if (Fail) throw new IOException("injected later-domain stage failure"); Writes++; return inner.WriteAsync(offset, source, token); }
        public ValueTask<long> GetLengthAsync(CancellationToken token = default) => inner.GetLengthAsync(token);
        public ValueTask SetLengthAsync(long length, CancellationToken token = default) => inner.SetLengthAsync(length, token);
        public ValueTask SyncAsync(CancellationToken token = default) { Syncs++; return inner.SyncAsync(token); }
        public ValueTask DisposeAsync() => inner.DisposeAsync();
    }
}
