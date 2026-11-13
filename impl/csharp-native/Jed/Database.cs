namespace Jed;

internal sealed record DatabaseSnapshot(Catalog Catalog, StorageMeta Meta, StoragePager? Pager = null)
{
    internal StorageEncryptedMeta? Encrypted { get; init; }
}
internal sealed class PendingDatabase(DatabaseSnapshot before)
{
    internal readonly DatabaseSnapshot Before = before;
    internal Catalog Catalog = before.Catalog;
    internal Dictionary<string, StorageTree> Trees = new(StringComparer.Ordinal);
    internal Dictionary<string, StorageTree> IndexTrees = new(StringComparer.Ordinal);
    internal HashSet<uint> RetiredPages = [];
    internal bool Changed;
    internal bool TrackMemory;
    internal long PendingBytes => Trees.Values.Concat(IndexTrees.Values).Aggregate(0L, (bytes, tree) => Meter.Add(bytes, tree.PendingBytes));
    internal void ForgetPendingKeys()
    {
        Trees = Trees.ToDictionary(item => item.Key, item => item.Value.ForgetPendingKeys(), StringComparer.Ordinal);
        IndexTrees = IndexTrees.ToDictionary(item => item.Key, item => item.Value.ForgetPendingKeys(), StringComparer.Ordinal);
    }
    internal SemaphoreSlim? WriterGate;
    internal StorageRecovered? StorageBefore;
    internal long Role;
    internal Database.FileWriteContext? FileWrite;
    internal PendingDatabase Fork() => new(Before) { Catalog = Catalog, Trees = new(Trees, StringComparer.Ordinal), IndexTrees = new(IndexTrees, StringComparer.Ordinal), RetiredPages = new(RetiredPages), Changed = Changed, TrackMemory = TrackMemory, WriterGate = WriterGate, StorageBefore = StorageBefore, Role = Role, FileWrite = FileWrite };
}

/// <summary>A shared database handle. Sessions serialize their own operations; readers retain a committed snapshot.</summary>
public sealed partial class Database : IAsyncDisposable
{
    private readonly object gate = new();
    internal SemaphoreSlim DomainPublication { get; } = new(1, 1);
    internal long MaxStorageBytes { get; set; }
    internal ScratchHost? ScratchHost { get; private set; }
    public const long DefaultWorkMem = 256L * 1024 * 1024;
    private long workMem = DefaultWorkMem;
    public long WorkMem => Interlocked.Read(ref workMem);
    /// <summary>Sets the per-operator spill budget; zero keeps operators resident.</summary>
    public void SetWorkMem(long bytes)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(bytes);
        Interlocked.Exchange(ref workMem, bytes);
    }
    private SemaphoreSlim writer = new(1, 1);
    private StoragePager pager;
    private StorageCommitter committer;
    private StoragePageCodec? ownedCodec;
    private StorageKeySelection? keyBlock;
    private DatabaseSnapshot current;
    private readonly Dictionary<ulong, int> pins = [];
    private readonly Dictionary<uint, ulong> retiredAt = [];
    private bool disposed;
    private bool ephemeral;
    private TaskCompletionSource? maintenance = null;

    public DatabaseEnvironment Environment { get; }

    private Database(StoragePager pager, StorageRecovered recovered, Catalog catalog, DatabaseEnvironment? environment = null)
    {
        Environment = environment ?? Jed.DatabaseEnvironment.Builder().Build();
        this.pager = pager; ownedCodec = pager.Codec; committer = new(pager, recovered); current = new(catalog, recovered.Meta) { Encrypted = recovered.Encrypted };
    }
    internal static Task<Database> CreateInMemoryAsync(int pageSize = 8192, CancellationToken cancellationToken = default, DatabaseEnvironment? environment = null)
        => CreateStoreAsync(new MemoryBlockStore(), pageSize, cancellationToken, environment);
    internal static async Task<Database> CreateStoreAsync(IBlockStore store, int pageSize = 8192, CancellationToken cancellationToken = default, DatabaseEnvironment? environment = null)
    {
        if (!StorageFormat.ValidPageSize(pageSize)) throw new ArgumentOutOfRangeException(nameof(pageSize));
        var meta = new StorageMeta(pageSize, 1, 2, 3, 0, 1);
        var bytes = StorageFormat.EncodeMeta(meta);
        try
        {
        await store.WriteAsync(0, bytes, cancellationToken).ConfigureAwait(false);
        await store.WriteAsync(pageSize, bytes, cancellationToken).ConfigureAwait(false);
        await store.WriteAsync(2L * pageSize, StorageFormat.EncodePage(pageSize, new(1, 0, 0, 0), []), cancellationToken).ConfigureAwait(false);
        return await OpenStoreAsync(store, cancellationToken, ephemeral: true, environment: environment).ConfigureAwait(false);
        }
        catch { await store.DisposeAsync().ConfigureAwait(false); throw; }
    }
    internal static async Task<Database> OpenStoreAsync(IBlockStore store, CancellationToken cancellationToken = default, bool ephemeral = false, DatabaseEnvironment? environment = null)
    {
        try
        {
            var recovered = await StorageRecovery.OpenAsync(store, cancellationToken).ConfigureAwait(false);
            var pager = new StoragePager(store, recovered.Meta.PageSize);
            var catalog = await Catalog.ReadAsync(pager, recovered.Meta, cancellationToken).ConfigureAwait(false);
            catalog = await StorageTree.ValidateCatalogSpinesAsync(pager, catalog, recovered.Meta, cancellationToken).ConfigureAwait(false);
            return new(pager, recovered, catalog, environment) { ephemeral = ephemeral };
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
            catalog = await StorageTree.ValidateCatalogSpinesAsync(pager, catalog, recovery.Meta.Header, cancellationToken).ConfigureAwait(false);
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
        fileState is null ? PinMemoryAsync(cancellationToken) : PinFileAsync(cancellationToken);
    private async ValueTask<SnapshotPin> PinMemoryAsync(CancellationToken token)
    {
        while (true)
        {
            token.ThrowIfCancellationRequested(); Task wait;
            lock (gate)
            {
                Check();
                if (maintenance is null) return Pin();
                wait = maintenance.Task;
            }
            await wait.WaitAsync(token).ConfigureAwait(false);
        }
    }
    internal SnapshotPin Pin()
    {
        lock (gate)
        {
            Check();
            if (fileState is not null) throw new InvalidOperationException("file snapshots require PinAsync");
            if (maintenance is not null) throw new SqlException(SqlStates.ObjectInUse, "database maintenance is active");
            var generation = current.Meta.TransactionId;
            pins[generation] = pins.GetValueOrDefault(generation) + 1;
            return new(this, current with { Pager = pager });
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
            internal readonly PageClientConnection? Connection = database.fileState?.Client;
            internal int References = 1;
        }
        private readonly Shared shared;
        internal SnapshotPin(Database database, DatabaseSnapshot snapshot, Func<ValueTask>? release = null) => shared = new(database, snapshot, release);
        private SnapshotPin(Shared shared) => this.shared = shared;
        internal DatabaseSnapshot Snapshot => shared.Snapshot;
        internal void CheckRole()
        {
            if (shared.Database.fileState is not null) shared.Database.CheckRole(shared.Role);
            if (shared.Connection is { IsConnected: false }) throw new SqlException(SqlStates.ConnectionFailure, "snapshot page-service connection lost");
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
    internal StorageTree Tree(CatalogTable table, PendingDatabase? pending, DatabaseSnapshot snapshot)
    {
        if (table.Domain is { } domain) return domain.Database.Tree(table with { Domain = null }, domain.Pending, domain.Snapshot);
        var result = pending is not null && pending.Trees.TryGetValue(table.Name, out var tree) ? tree :
            new(snapshot.Pager ?? pager, table.LeafSchema(), table.Root, snapshot.Meta.PageCount, table.RootTag, table.Shape, table.ResidentRoot);
        return pending is null ? result : result.WithPendingTracking(pending.TrackMemory);
    }
    internal StorageTree IndexTree(CatalogIndex index, PendingDatabase? pending, DatabaseSnapshot snapshot)
    {
        if (index.Domain is { } domain) return domain.Database.IndexTree(index with { Domain = null }, domain.Pending, domain.Snapshot);
        var result = pending is not null && pending.IndexTrees.TryGetValue(index.Name, out var tree) ? tree :
            index.Kind == 2 ? index.Root == 0 ? new(snapshot.Pager ?? pager, new StorageLeafSchema([])) : index.Gist?.Leaves ?? throw StorageFormat.Corrupt("GiST index was not loaded") :
            new(snapshot.Pager ?? pager, new StorageLeafSchema([]), index.Root, snapshot.Meta.PageCount, index.RootTag, index.Shape);
        return pending is null ? result : result.WithPendingTracking(pending.TrackMemory);
    }

    internal async ValueTask CommitAsync(PendingDatabase pending, CancellationToken cancellationToken)
    {
        CheckPendingRole(pending);
        if (!pending.Changed) return;
        var prepared = await PrepareCommitAsync(pending, "main", cancellationToken, durability: true).ConfigureAwait(false);
        await DomainPublication.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            if (ephemeral)
            {
                await committer.StageMemoryAsync(prepared.Plan, cancellationToken).ConfigureAwait(false);
                committer.AdoptStagedMemory(prepared.Plan); AdoptPreparedCommit(prepared);
            }
            else
            {
                using var publication = await CommitFileOrMemoryAsync(pending, prepared.Plan, cancellationToken).ConfigureAwait(false);
                if (pending.FileWrite?.ResolvedAfterRoleChange != true) AdoptPreparedCommit(prepared);
            }
        }
        finally { DomainPublication.Release(); }
    }
    internal async ValueTask ValidatePendingStorageAsync(PendingDatabase pending, string name, CancellationToken cancellationToken = default)
    {
        if (!pending.Changed || MaxStorageBytes <= 0) return;
        if (name == "temp")
        {
            if ((long)pending.Before.Meta.PageCount * pending.Before.Meta.PageSize > MaxStorageBytes)
                throw new SqlException(SqlStates.TempStorageLimitExceeded, "temporary storage exceeded its limit");
        }
        else _ = await PrepareCommitAsync(pending, name, cancellationToken).ConfigureAwait(false);
    }
    private sealed record PreparedCommit(PendingDatabase Pending, Catalog Catalog, StorageCommitPlan Plan);
    private async ValueTask<PreparedCommit> PrepareCommitAsync(PendingDatabase pending, string name, CancellationToken cancellationToken, bool compacted = false, bool durability = false)
    {
        CheckPendingRole(pending);
        long? deadline = null;
        if (durability && !ephemeral && pending.FileWrite is null)
        {
            deadline = await PrepareCommitPolicyAsync(cancellationToken).ConfigureAwait(false);
            pending.StorageBefore = committer.Current;
        }
        var beforeStorage = pending.StorageBefore ?? committer.Current;
        HashSet<uint> reuse; bool canReclaim;
        lock (gate)
        {
            Check();
            var oldest = pins.Count == 0 ? ulong.MaxValue : pins.Keys.Min();
            if (fileState?.Ledger?.OldestPin is { } remoteOldest) oldest = Math.Min(oldest, remoteOldest);
            // The new free list describes the generation being committed.
            // A reader of the previous current root still pins an older version.
            canReclaim = oldest > beforeStorage.Meta.TransactionId;
            reuse = !ephemeral ? oldest >= beforeStorage.FreeGeneration ? new(beforeStorage.FreePages) : []
                : beforeStorage.FreePages.Where(page => !retiredAt.TryGetValue(page, out var at) || at <= oldest).ToHashSet();
            // Raw, host-coordinated file attachments use the frozen conservative
            // policy: their reader watermark must drain before any reuse.
            // Auto/service attachments retain the ordinary free-generation gate.
            if (!ephemeral && name is not ("main" or "temp") && !canReclaim &&
                fileState is { Options: { Locking: FileLocking.None, RoleHost: null } }) reuse.Clear();
        }
        if (pending.FileWrite is { } remote) reuse = remote.Granted;
        var allocation = new StorageAllocation(beforeStorage, reuse, (pending.Before.Pager ?? pager).Codec, eagerReclaim: ephemeral);
        long delta = -pending.RetiredPages.Count;
        foreach (var page in pending.RetiredPages) allocation.Reclaim(page);
        var tables = new List<CatalogTable>();
        foreach (var pendingTable in pending.Catalog.Tables.OrderBy(t => t.Name, Utf8NameComparer.Instance))
        {
            var table = pendingTable with { Domain = null };
            if (pending.Trees.TryGetValue(table.Name, out var tree))
            {
                delta += tree.PageDelta;
                var root = await tree.SerializeBoundAsync(allocation, cancellationToken).ConfigureAwait(false);
                table = table with { Root = root.Page, RootTag = root.Tag, Shape = tree.SerializedShape, ResidentRoot = tree.SerializedRootRecords };
            }
            var indexes = new List<CatalogIndex>();
            foreach (var index in (table.Indexes ?? []).OrderBy(item => item.Name, Utf8NameComparer.Instance))
            {
                if (pending.IndexTrees.TryGetValue(index.Name, out var indexTree))
                {
                    if (index.Kind == 2)
                    {
                        var gist = await GistTree.BuildAsync(indexTree, index.Keys.Select(key => table.Columns[key.Column].Type.Type).ToArray(), cancellationToken).ConfigureAwait(false);
                        var oldPages = index.Gist?.Pages ?? [];
                        foreach (var page in oldPages) if (!pending.RetiredPages.Contains(page)) { allocation.Reclaim(page); delta--; }
                        var serialized = gist.Serialize(allocation); delta += serialized.Pages.Length;
                        indexes.Add(index with { Root = serialized.Root.Page, RootTag = serialized.Root.Tag, Gist = gist with { Leaves = gist.Leaves.CommittedResidentCopy(), Pages = serialized.Pages }, Domain = null });
                        continue;
                    }
                    delta += indexTree.PageDelta;
                    var indexRoot = await indexTree.SerializeBoundAsync(allocation, cancellationToken).ConfigureAwait(false);
                    indexes.Add(index with { Root = indexRoot.Page, RootTag = indexRoot.Tag, Shape = indexTree.SerializedShape, Domain = null });
                }
                else indexes.Add(index with { Domain = null });
            }
            tables.Add(table with { Indexes = indexes.ToArray() });
        }
        var catalog = pending.Catalog with { Tables = tables.ToArray(), Scope = null };
        var encoded = catalog.Serialize(allocation); delta += encoded.PageDelta;
        var live = checked((uint)(beforeStorage.Meta.LivePages + delta));
        if (!ephemeral && pending.FileWrite is null && canReclaim &&
            (beforeStorage.ReclamationLive == 0 || allocation.OffFreeListCount > ReclamationFacts.CompactMinPages &&
            allocation.OffFreeListCount > ReclamationFacts.CompactGrowth * beforeStorage.ReclamationLive))
        {
            var plannedMeta = new StorageMeta(allocation.PageSize, checked(beforeStorage.Meta.TransactionId + 1),
                encoded.Root, allocation.PageCount, 0, live);
            var plannedSnapshot = new DatabaseSnapshot(catalog with { Pages = encoded.Pages }, plannedMeta)
            { Encrypted = allocation.PlannedEncryption(plannedMeta) };
            var reachable = await new StorageSnapshotCopy(allocation.ReadView(pending.Before.Pager ?? pager), plannedSnapshot)
                .ReachableAsync(cancellationToken).ConfigureAwait(false);
            allocation.CollectUnreachable(reachable, cancellationToken);
        }
        var plan = ephemeral ? allocation.PrepareMemory(encoded.Root, live) : allocation.Prepare(encoded.Root, live, truncate: pending.FileWrite is null);
        if (name != "temp" && MaxStorageBytes > 0)
        {
            var grows = fileState is null ? plan.Meta.PageCount > plan.Before.Meta.PageCount : plan.Meta.LivePages > plan.Before.Meta.LivePages;
            var pages = fileState is null ? plan.Meta.PageCount : plan.Meta.LivePages;
            if (grows && (long)pages * plan.Meta.PageSize > MaxStorageBytes)
            {
                if (fileState is null && !compacted && await ReclaimMemoryOrphansAsync(pending, cancellationToken).ConfigureAwait(false))
                    return await PrepareCommitAsync(pending, name, cancellationToken, compacted: true).ConfigureAwait(false);
                var repair = pending.PendingBytes == 0 && encoded.Pages.Length <= pending.Before.Catalog.Pages.Length;
                if (!repair) throw new SqlException(SqlStates.StorageLimitExceeded, $"storage of database \"{name}\" exceeded the limit of {MaxStorageBytes} bytes");
            }
        }
        return new(pending, catalog with { Pages = encoded.Pages }, plan with { DeadlineMicroseconds = deadline });
    }
    private void AdoptPreparedCommit(PreparedCommit prepared)
    {
        var plan = prepared.Plan;
        lock (gate)
        {
            foreach (var page in plan.FreePages)
                if (!plan.Before.FreePages.Contains(page)) retiredAt[page] = plan.Meta.TransactionId;
            foreach (var page in retiredAt.Keys.Where(page => !plan.FreePages.Contains(page)).ToArray()) retiredAt.Remove(page);
            current = new(prepared.Catalog, plan.Meta, prepared.Pending.Before.Pager) { Encrypted = plan.Encrypted };
            if (fileState?.Ledger is { } ledger)
            {
                ledger.Publish(plan.MetaBytes, plan.Body.Select(page => page.Page).Concat(plan.Manifest.Select(page => page.Page)));
                fileState.CatalogDirty = false;
            }
        }
        FinishCommitPolicy();
    }
    private void Check()
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (fileState?.Failure is { } failure) throw failure;
        if (committer.IsPoisoned) throw new SqlException(SqlStates.IoError, "database handle requires recovery");
    }
    public async ValueTask DisposeAsync()
    {
        lock (gate)
        {
            if (disposed && attachments.Count == 0) return;
            // Local sessions must end before close. A remote writer instead
            // belongs to the page service and drains through its shutdown grace.
            if (!disposed && (domainLeases != 0 || attaching.Count != 0 || pins.Count != 0 ||
                writer.CurrentCount == 0 && !ReferenceEquals(fileState?.RemoteWriterGate, writer)))
                throw new SqlException(SqlStates.ObjectInUse, "database has active local operations");
            attachmentsClosing = true;
        }
        if (fileState is not null)
        {
            try { await DisposeFileAsync().ConfigureAwait(false); }
            finally { await DisposeAttachedAsync().ConfigureAwait(false); }
            return;
        }
        await DisposeAttachedAsync().ConfigureAwait(false);
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
