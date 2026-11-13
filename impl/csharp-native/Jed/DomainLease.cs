namespace Jed;

// A lease owns both admission and all reader watermarks. Writer gates can end at
// COMMIT while a cursor retains an independent reference to the pinned pages.
internal sealed class DomainLease : IDisposable, IAsyncDisposable
{
    private sealed class Shared(Database root)
    {
        internal readonly Database Root = root;
        internal readonly List<(Database Database, PendingDatabase Pending)> Writers = [];
        internal readonly Dictionary<Database, Database.SnapshotPin> Pins = [];
        internal readonly List<PageClientStore> RetainedStores = [];
        internal System.Runtime.ExceptionServices.ExceptionDispatchInfo? ProtectionFailure;
        internal int References = 1;
        internal bool WritersReleased;
    }
    private readonly Shared shared;
    private int disposed;
    internal DatabaseDomains Scope { get; }
    private DomainLease(Shared shared, DatabaseDomains scope) { this.shared = shared; Scope = scope; }
    internal DomainLease Retain()
    {
        lock (shared)
        {
            if (disposed != 0) throw new ObjectDisposedException(nameof(DomainLease));
            shared.References++; return new(shared, Scope);
        }
    }
    internal void CheckRoles()
    {
        shared.ProtectionFailure?.Throw();
        foreach (var pin in shared.Pins.Values) pin.CheckRole();
        if (!shared.WritersReleased)
            foreach (var writer in shared.Writers) writer.Database.CheckPendingRole(writer.Pending);
    }
    internal static async ValueTask<DomainLease> Acquire(Database root, Database? temporary, bool writePersistent, bool writeTemporary, CancellationToken token)
    {
        var registry = root.AcquireAttachmentRegistry(); var shared = new Shared(root);
        try
        {
            var entries = new List<AttachedDatabase> { new("main", root, root.DomainReadOnly) };
            entries.AddRange(registry);
            if (temporary is not null) entries.Add(new("temp", temporary, false));
            var writable = entries.Where(entry => entry.Name == "temp" ? writeTemporary : writePersistent && !entry.ReadOnly).OrderBy(entry => entry.Database.DomainLockOrder, StringComparer.Ordinal).ToArray();
            if (writePersistent && root.DomainReadOnly) throw new SqlException(SqlStates.ReadOnlySqlTransaction, "database handle is read-only");
            foreach (var entry in writable)
                shared.Writers.Add((entry.Database, await entry.Database.BeginWriteAsync(token).ConfigureAwait(false)));
            await root.DomainPublication.WaitAsync(token).ConfigureAwait(false);
            var domains = new List<DatabaseDomain>();
            try
            {
                foreach (var entry in entries)
                {
                    var pending = shared.Writers.FirstOrDefault(writer => ReferenceEquals(writer.Database, entry.Database)).Pending;
                    DatabaseSnapshot snapshot;
                    if (pending is not null) snapshot = pending.Before;
                    else
                    {
                        var pin = await entry.Database.PinAsync(token).ConfigureAwait(false);
                        shared.Pins.Add(entry.Database, pin); snapshot = pin.Snapshot;
                    }
                    domains.Add(new(entry.Name, entry.Database, snapshot, pending, entry.ReadOnly));
                }
            }
            finally { root.DomainPublication.Release(); }
            return new(shared, new(domains));
        }
        catch
        {
            try { await new DomainLease(shared, new([])).DisposeAsync().ConfigureAwait(false); }
            catch (Exception) { } // Preserve the acquisition failure after releasing every resource.
            throw;
        }
    }
    // The writer grant protects these roots until publication. Only a cursor
    // that outlives that grant needs an additional independent reader pin.
    internal async ValueTask PrepareWriterPinsForCommitAsync()
    {
        shared.ProtectionFailure?.Throw();
        try
        {
            foreach (var writer in shared.Writers)
            {
                lock (shared)
                    if (shared.References <= 1 || shared.Pins.ContainsKey(writer.Database)) continue;
                writer.Database.CheckPendingRole(writer.Pending);
                var pin = await writer.Database.PinAsync().ConfigureAwait(false);
                try { writer.Database.CheckPendingRole(writer.Pending); }
                catch { await pin.DisposeAsync().ConfigureAwait(false); throw; }
                if (pin.Snapshot.Meta.TransactionId != writer.Pending.Before.Meta.TransactionId)
                {
                    await pin.DisposeAsync().ConfigureAwait(false);
                    throw new InvalidOperationException("retained writer roots changed before reader pin admission");
                }
                var keep = false;
                lock (shared)
                {
                    if (shared.References > 1)
                    {
                        shared.Pins.Add(writer.Database, pin); keep = true;
                        // Existing cursors retain the writer's pager, not the new
                        // reader pager. Keep its byte store alive under this pin.
                        if (writer.Pending.FileWrite is { Store: { } store } context)
                        { context.Store = null; shared.RetainedStores.Add(store); }
                    }
                }
                if (!keep) await pin.DisposeAsync().ConfigureAwait(false);
            }
        }
        catch (Exception error)
        {
            // A surviving cursor must fail closed when its old roots could not
            // be protected, even after every writer grant has been released.
            lock (shared) shared.ProtectionFailure ??= System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(error);
            throw;
        }
    }
    internal async ValueTask ReleaseWriters()
    {
        Exception? failure = null;
        try { if (!shared.WritersReleased) await PrepareWriterPinsForCommitAsync().ConfigureAwait(false); }
        catch (Exception error) { failure = error; }
        lock (shared)
        {
            if (shared.WritersReleased) return;
            shared.WritersReleased = true;
        }
        foreach (var writer in shared.Writers.AsEnumerable().Reverse())
            try { await writer.Database.EndWriteAsync(writer.Pending).ConfigureAwait(false); }
            catch (Exception error) { failure ??= error; }
        if (failure is not null) System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(failure).Throw();
    }
    public void Dispose() { _ = DisposeAsync(); }
    public async ValueTask DisposeAsync()
    {
        lock (shared)
        {
            if (disposed != 0) return;
            disposed = 1;
            if (--shared.References != 0) return;
        }
        Exception? failure = null;
        try { await ReleaseWriters().ConfigureAwait(false); } catch (Exception error) { failure = error; }
        foreach (var pin in shared.Pins.Values)
            try { await pin.DisposeAsync().ConfigureAwait(false); }
            catch (Exception error) { failure ??= error; }
        foreach (var store in shared.RetainedStores)
            try { await store.DisposeAsync().ConfigureAwait(false); }
            catch (Exception error) { failure ??= error; }
        shared.Root.ReleaseAttachmentRegistry();
        if (failure is not null) System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(failure).Throw();
    }
}
