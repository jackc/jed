namespace Jed;

internal sealed record DatabaseSnapshot(Catalog Catalog, StorageMeta Meta, StoragePager? Pager = null);
internal sealed class PendingDatabase(DatabaseSnapshot before)
{
    internal readonly DatabaseSnapshot Before = before;
    internal Catalog Catalog = before.Catalog;
    internal Dictionary<string, StorageTree> Trees = new(StringComparer.Ordinal);
    internal Dictionary<string, StorageTree> IndexTrees = new(StringComparer.Ordinal);
    internal HashSet<uint> RetiredPages = [];
    internal bool Changed;
    internal SemaphoreSlim? WriterGate;
    internal StorageRecovered? StorageBefore;
    internal long Role;
    internal Database.FileWriteContext? FileWrite;
    internal PendingDatabase Fork() => new(Before) { Catalog = Catalog, Trees = new(Trees, StringComparer.Ordinal), IndexTrees = new(IndexTrees, StringComparer.Ordinal), RetiredPages = new(RetiredPages), Changed = Changed, WriterGate = WriterGate, StorageBefore = StorageBefore, Role = Role, FileWrite = FileWrite };
}

/// <summary>A shared database handle. Sessions serialize their own operations; readers retain a committed snapshot.</summary>
public sealed partial class Database : IAsyncDisposable
{
    private readonly object gate = new();
    private SemaphoreSlim writer = new(1, 1);
    private StoragePager pager;
    private StorageCommitter committer;
    private readonly StoragePageCodec? ownedCodec;
    private StorageKeySelection? keyBlock;
    private DatabaseSnapshot current;
    private readonly Dictionary<ulong, int> pins = [];
    private readonly Dictionary<uint, ulong> retiredAt = [];
    private bool disposed;

    private Database(StoragePager pager, StorageRecovered recovered, Catalog catalog)
    {
        this.pager = pager; ownedCodec = pager.Codec; committer = new(pager, recovered); current = new(catalog, recovered.Meta);
    }
    public static async Task<Database> CreateInMemoryAsync(int pageSize = 8192, CancellationToken cancellationToken = default)
    {
        if (!StorageFormat.ValidPageSize(pageSize)) throw new ArgumentOutOfRangeException(nameof(pageSize));
        var meta = new StorageMeta(pageSize, 1, 2, 3, 0, 1);
        var store = new MemoryBlockStore();
        var bytes = StorageFormat.EncodeMeta(meta);
        await store.WriteAsync(0, bytes, cancellationToken).ConfigureAwait(false);
        await store.WriteAsync(pageSize, bytes, cancellationToken).ConfigureAwait(false);
        await store.WriteAsync(2L * pageSize, StorageFormat.EncodePage(pageSize, new(1, 0, 0, 0), []), cancellationToken).ConfigureAwait(false);
        return await OpenStoreAsync(store, cancellationToken).ConfigureAwait(false);
    }
    internal static async Task<Database> OpenStoreAsync(IBlockStore store, CancellationToken cancellationToken = default)
    {
        try
        {
            var recovered = await StorageRecovery.OpenAsync(store, cancellationToken).ConfigureAwait(false);
            var pager = new StoragePager(store, recovered.Meta.PageSize);
            var catalog = await Catalog.ReadAsync(pager, recovered.Meta, cancellationToken).ConfigureAwait(false);
            return new(pager, recovered, catalog);
        }
        catch { await store.DisposeAsync().ConfigureAwait(false); throw; }
    }
    internal static async Task<Database> OpenEncryptedStoreAsync(IBlockStore store, StorageCredential? credential,
        Func<int, byte[]> entropy, CancellationToken cancellationToken = default)
    {
        StorageEncryptedRecovery? recovery = null;
        try
        {
            recovery = await StorageCryptoRecovery.OpenAsync(store, credential, entropy, cancellationToken).ConfigureAwait(false);
            var pager = new StoragePager(store, recovery.Meta.Header.PageSize, codec: recovery.Codec);
            var catalog = await Catalog.ReadAsync(pager, recovery.Meta, cancellationToken).ConfigureAwait(false);
            return new(pager, recovery.Recovered, catalog) { keyBlock = recovery.KeyBlock };
        }
        catch { recovery?.Dispose(); await store.DisposeAsync().ConfigureAwait(false); throw; }
    }
    public Session CreateSession(long lifetimeMaxCost = 0)
    {
        lock (gate) { Check(); return new(this, lifetimeMaxCost); }
    }
    public Session CreateSession(SessionOptions options)
    {
        lock (gate) { Check(); return new(this, options); }
    }
    internal ValueTask<SnapshotPin> PinAsync(CancellationToken cancellationToken = default) =>
        fileState is null ? ValueTask.FromResult(Pin()) : PinFileAsync(cancellationToken);
    internal SnapshotPin Pin()
    {
        lock (gate)
        {
            Check();
            if (fileState is not null) throw new InvalidOperationException("file snapshots require PinAsync");
            var generation = current.Meta.TransactionId;
            pins[generation] = pins.GetValueOrDefault(generation) + 1;
            return new(this, current);
        }
    }
    internal sealed class SnapshotPin : IDisposable, IAsyncDisposable
    {
        private sealed class Shared(Database database, DatabaseSnapshot snapshot, Func<ValueTask>? release)
        {
            internal readonly Database Database = database;
            internal readonly DatabaseSnapshot Snapshot = snapshot;
            internal readonly Func<ValueTask>? Release = release;
            internal readonly long Role = database.fileState?.Role ?? 0;
            internal int References = 1;
        }
        private readonly Shared shared;
        internal SnapshotPin(Database database, DatabaseSnapshot snapshot, Func<ValueTask>? release = null) => shared = new(database, snapshot, release);
        private SnapshotPin(Shared shared) => this.shared = shared;
        internal DatabaseSnapshot Snapshot => shared.Snapshot;
        internal void CheckRole()
        {
            if (shared.Database.fileState is not null) shared.Database.CheckRole(shared.Role);
        }
        private Task? releasing;
        internal SnapshotPin Retain()
        {
            lock (shared)
            {
                ObjectDisposedException.ThrowIf(releasing is not null, this);
                checked { shared.References++; }
                return new(shared);
            }
        }
        public void Dispose() { _ = DisposeAsync(); }
        public ValueTask DisposeAsync()
        {
            lock (shared)
            {
                if (releasing is not null) return new(releasing);
                if (--shared.References != 0) { releasing = Task.CompletedTask; return ValueTask.CompletedTask; }
                var database = shared.Database;
                lock (database.gate)
                {
                    var generation = shared.Snapshot.Meta.TransactionId;
                    if ((database.fileState?.Role ?? 0) == shared.Role && --database.pins[generation] == 0) database.pins.Remove(generation);
                }
                releasing = shared.Release is null ? Task.CompletedTask : shared.Release().AsTask();
                return new(releasing);
            }
        }
    }
    internal async ValueTask<PendingDatabase> BeginWriteAsync(CancellationToken cancellationToken)
    {
        if (fileState is not null) return await BeginFileWriteAsync(cancellationToken).ConfigureAwait(false);
        await writer.WaitAsync(cancellationToken).ConfigureAwait(false);
        try { lock (gate) { Check(); return new(current) { WriterGate = writer, StorageBefore = committer.Current }; } }
        catch { writer.Release(); throw; }
    }
    internal async ValueTask EndWriteAsync(PendingDatabase pending)
    {
        try { if (pending.FileWrite is not null) await pending.FileWrite.DisposeAsync().ConfigureAwait(false); }
        finally { pending.WriterGate!.Release(); }
    }
    internal StorageTree Tree(CatalogTable table, PendingDatabase? pending, DatabaseSnapshot snapshot) =>
        pending is not null && pending.Trees.TryGetValue(table.Name, out var tree) ? tree : new(snapshot.Pager ?? pager, table.LeafSchema(), table.Root, snapshot.Meta.PageCount, table.RootTag);

    internal StorageTree IndexTree(CatalogIndex index, PendingDatabase? pending, DatabaseSnapshot snapshot) =>
        pending is not null && pending.IndexTrees.TryGetValue(index.Name, out var tree) ? tree :
            new(snapshot.Pager ?? pager, new StorageLeafSchema([]), index.Root, snapshot.Meta.PageCount, index.RootTag);

    internal async ValueTask CommitAsync(PendingDatabase pending, CancellationToken cancellationToken)
    {
        CheckPendingRole(pending);
        if (!pending.Changed) return;
        var beforeStorage = pending.StorageBefore ?? committer.Current;
        HashSet<uint> reuse;
        lock (gate)
        {
            Check();
            var oldest = pins.Count == 0 ? ulong.MaxValue : pins.Keys.Min();
            if (fileState?.Ledger?.OldestPin is { } remoteOldest) oldest = Math.Min(oldest, remoteOldest);
            reuse = beforeStorage.FreePages.Where(page => !retiredAt.TryGetValue(page, out var at) || at <= oldest).ToHashSet();
        }
        if (pending.FileWrite is { } remote) reuse = remote.Granted;
        var allocation = new StorageAllocation(beforeStorage, reuse, (pending.Before.Pager ?? pager).Codec);
        long delta = -pending.RetiredPages.Count;
        foreach (var page in pending.RetiredPages) allocation.Reclaim(page);
        var tables = new List<CatalogTable>();
        foreach (var pendingTable in pending.Catalog.Tables.OrderBy(t => t.Name, Utf8NameComparer.Instance))
        {
            var table = pendingTable;
            if (pending.Trees.TryGetValue(table.Name, out var tree))
            {
                delta += tree.PageDelta;
                var root = await tree.SerializeBoundAsync(allocation, cancellationToken).ConfigureAwait(false);
                table = table with { Root = root.Page, RootTag = root.Tag };
            }
            var indexes = new List<CatalogIndex>();
            foreach (var index in (table.Indexes ?? []).OrderBy(item => item.Name, Utf8NameComparer.Instance))
            {
                if (pending.IndexTrees.TryGetValue(index.Name, out var indexTree))
                {
                    delta += indexTree.PageDelta;
                    var indexRoot = await indexTree.SerializeBoundAsync(allocation, cancellationToken).ConfigureAwait(false);
                    indexes.Add(index with { Root = indexRoot.Page, RootTag = indexRoot.Tag });
                }
                else indexes.Add(index);
            }
            tables.Add(table with { Indexes = indexes.ToArray() });
        }
        var catalog = pending.Catalog with { Tables = tables.ToArray() };
        var encoded = catalog.Serialize(allocation); delta += encoded.PageDelta;
        var live = checked((uint)(beforeStorage.Meta.LivePages + delta));
        var plan = allocation.Prepare(encoded.Root, live);
        var previouslyFree = beforeStorage.FreePages;
        using var publication = await CommitFileOrMemoryAsync(pending, plan, cancellationToken).ConfigureAwait(false);
        if (pending.FileWrite?.ResolvedAfterRoleChange == true) return;
        lock (gate)
        {
            foreach (var page in plan.FreePages)
                if (!previouslyFree.Contains(page)) retiredAt[page] = plan.Meta.TransactionId;
            foreach (var page in retiredAt.Keys.Where(page => !plan.FreePages.Contains(page)).ToArray()) retiredAt.Remove(page);
            current = new(catalog with { Pages = encoded.Pages }, plan.Meta, pending.Before.Pager);
            if (fileState?.Ledger is { } ledger)
            {
                ledger.Publish(plan.MetaBytes, plan.Body.Select(page => page.Page).Concat(plan.Manifest.Select(page => page.Page)));
                fileState.CatalogDirty = false;
            }
        }
    }
    private void Check()
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (fileState?.Failure is { } failure) throw failure;
        if (committer.IsPoisoned) throw new SqlException(SqlStates.IoError, "database handle requires recovery");
    }
    public async ValueTask DisposeAsync()
    {
        if (fileState is not null) { await DisposeFileAsync().ConfigureAwait(false); return; }
        lock (gate)
        {
            if (disposed) return;
            if (pins.Count != 0 || !writer.Wait(0)) throw new SqlException(SqlStates.ObjectInUse, "database has active readers or writer");
            disposed = true;
        }
        try { await pager.Store.DisposeAsync().ConfigureAwait(false); }
        finally { ownedCodec?.Dispose(); writer.Release(); }
    }
}
