using System.Net.Sockets;
using Jed;

internal static class PageOwnerTests
{
    internal static async Task Run()
    {
        const int size = 256;
        var initial = new StorageMeta(size, 1, 2, 3, 0, 1);
        var rawMeta = StorageFormat.EncodeMeta(initial);
        var rawRoot = StorageFormat.EncodePage(size, new(1, 0, 0, 0), []);
        var recovered = new StorageRecovered(initial, 1, 1, [], [], []);
        var allocation = new StorageAllocation(recovered);
        var root = allocation.AddPage(rawRoot); var plan = allocation.Prepare(root, 1);
        var grant = new HashSet<uint>(); var pending = new Dictionary<uint, ulong>();
        var manifest = plan.Manifest.Select(page => new PageWirePage(page.Page, page.Bytes)).ToArray();
        var validator = new PageCommitValidator(initial, false, grant, pending, 2, plan.MetaBytes, manifest);
        Reject(() => validator.Complete(), "Check C incomplete body");
        var altered = plan.Body[0].Bytes.ToArray(); altered[16] ^= 1;
        Reject(() => validator.Receive(plan.Body[0].Page, altered), "Check B digest");
        foreach (var page in plan.Body) validator.Receive(page.Page, page.Bytes);
        Reject(() => validator.Receive(plan.Body[0].Page, plan.Body[0].Bytes), "Check B duplicate"); validator.Complete();
        Reject(() => _ = new PageCommitValidator(initial, false, grant, pending, 3, plan.MetaBytes, manifest), "Check A generation");
        Reject(() => _ = new PageCommitValidator(initial with { PageCount = 4 }, false, grant, pending, 2, plan.MetaBytes, manifest), "Check A ungranted overwrite");

        var ledger = new PageOwnerLedger(new object(), new byte[16], rawMeta);
        var path = Path.Combine(Path.GetTempPath(), "jed-csharp-owner-" + Guid.NewGuid().ToString("N") + ".sock");
        await using var store = new MemoryBlockStore();
        await store.WriteAsync(0, rawMeta); await store.WriteAsync(size, rawMeta); await store.WriteAsync(2L * size, rawRoot);
        using var gate = new SemaphoreSlim(1);
        var backend = new PageOwnerBackend(ledger,
            async (index, token) => { var bytes = new byte[size]; await store.ReadAsync((long)index * size, bytes, token); return bytes; },
            async (relaxed, token) =>
            {
                if (relaxed) throw new SqlException(SqlStates.FeatureNotSupported, "test adapter supports durable commits only");
                await gate.WaitAsync(token);
                return new TestWrite(store, ledger, initial, gate);
            },
            _ => ValueTask.CompletedTask,
            (_, _) => ValueTask.FromResult((byte)1));
        await using var server = new PageOwnerServer(backend, new(IdleTransactionMilliseconds: 0));
        using var timeout = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        try
        {
            server.Listen(path);
            await using var client = await PageClientConnection.ConnectLocalAsync(path, cancellationToken: timeout.Token);
            var read = await client.BeginReadAsync(new byte[16], 0, timeout.Token);
            Corpus.Equal(1, ledger.PinCount, "owner counted client read pin");
            Corpus.Equal(1ul, read.TransactionId, "owner snapshot");
            var pages = await client.ReadPagesAsync(1, [2], timeout.Token);
            Equal(rawRoot, pages[0], "owner returns exact stored body");
            await client.EndReadAsync(1, timeout.Token);
            var write = await client.BeginWriteAsync(read.Incarnation, 1, 1000, cancellationToken: timeout.Token);
            Corpus.Equal(false, write.Snapshot.InvalidateAll, "same generation exact cache");
            Corpus.Equal(2ul, await client.CommitAsync(plan, timeout.Token), "remote commit result");
            Corpus.Equal((byte)0, await client.CommitStatusAsync(2, PageOwnerLedger.ResolutionDigest(plan.MetaBytes), timeout.Token), "commit resolution committed");
            Corpus.Equal((byte)1, await client.CommitStatusAsync(2, 0, timeout.Token), "commit resolution differing digest");
            Corpus.Equal((byte)1, await client.CommitStatusAsync(3, 0, timeout.Token), "future commit not committed");
            var updated = await client.BeginReadAsync(read.Incarnation, 1, timeout.Token);
            Corpus.Equal(2ul, updated.TransactionId, "new owner generation");
            Corpus.Equal(string.Join(',', validator.WrittenIndices.Order()), string.Join(',', updated.Invalidated), "exact invalidation list");
            await client.EndReadAsync(2, timeout.Token);
            Corpus.Equal((byte)1, await client.RequestOwnershipAsync(timeout.Token), "prefer owner refuses handoff");
            await using var readOnly = await PageClientConnection.ConnectLocalAsync(path, true, timeout.Token);
            try { _ = await readOnly.BeginWriteAsync(read.Incarnation, 2, 0, cancellationToken: timeout.Token); throw new InvalidDataException("read-only writer accepted"); }
            catch (SqlException error) when (error.SqlState == SqlStates.ReadOnlySqlTransaction) { }
            // Invalid body pointers get a fatal error and release all connection pins.
            var readOnlyPin = await readOnly.BeginReadAsync(read.Incarnation, 2, timeout.Token);
            try { _ = await readOnly.ReadPagesAsync(readOnlyPin.TransactionId, [1], timeout.Token); throw new InvalidDataException("meta read accepted as body"); }
            catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { }
        }
        finally { await server.DisposeAsync(); File.Delete(path); }
        Corpus.Equal(0, ledger.PinCount, "connection disposal releases all pins");
        Console.WriteLine("PASS page-owner host adapter: real client reads, writer lease, streamed Check A/B/C, durable publication, cache invalidation, resolution and pin cleanup");
    }
    private sealed class TestWrite(MemoryBlockStore store, PageOwnerLedger ledger, StorageMeta before, SemaphoreSlim gate) : IPageOwnerWrite
    {
        private PageCommitValidator? validation;
        private int disposed;
        public PageOwnerLedger.PageOwnerPin Pin { get; } = ledger.Pin(ledger.Incarnation, ledger.CurrentTransactionId);
        public bool ReuseGrant => false;
        public PageWireDigest[] Pending => [];
        public uint[] Reuse => [];
        public async ValueTask BeginCommitAsync(ulong version, byte[] meta, PageWirePage[] manifest, CancellationToken cancellationToken)
        {
            validation = new(before, false, new HashSet<uint>(), new Dictionary<uint, ulong>(), version, meta, manifest);
            foreach (var page in manifest) await store.WriteAsync((long)page.Index * before.PageSize, page.Bytes, cancellationToken);
        }
        public async ValueTask WritePagesAsync(PageWirePage[] pages, CancellationToken cancellationToken)
        {
            foreach (var page in pages) { validation!.Receive(page.Index, page.Bytes); await store.WriteAsync((long)page.Index * before.PageSize, page.Bytes, cancellationToken); }
        }
        public async ValueTask<ulong> CompleteCommitAsync(CancellationToken cancellationToken)
        {
            validation!.Complete(); ledger.BeginPublication();
            var meta = validation.MetaForSlot(0);
            await store.WriteAsync(0, meta, cancellationToken); await store.SyncAsync(cancellationToken);
            ledger.Publish(meta, validation.WrittenIndices); return validation.Meta.TransactionId;
        }
        public ValueTask DisposeAsync()
        {
            if (Interlocked.Exchange(ref disposed, 1) == 0) { Pin.Dispose(); gate.Release(); }
            return ValueTask.CompletedTask;
        }
    }
    private static void Reject(Action action, string name)
    {
        try { action(); } catch (SqlException error) when (error.SqlState == SqlStates.ProtocolViolation) { return; }
        throw new InvalidDataException("accepted invalid commit delta: " + name);
    }
    private static void Equal(byte[] expected, byte[] actual, string name) => Corpus.Equal(Convert.ToHexString(expected), Convert.ToHexString(actual), name);
}
