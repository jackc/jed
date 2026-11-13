namespace Jed;

// A lease owns both admission and all reader watermarks. Writer gates can end at
// COMMIT while a cursor retains an independent reference to the pinned pages.
internal sealed class DomainLease : IDisposable, IAsyncDisposable
{
    private sealed class Shared(Database root)
    {
        internal readonly Database Root = root;
        internal readonly List<(Database Database, PendingDatabase Pending)> Writers = [];
        internal readonly List<Database.SnapshotPin> Pins = [];
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
    internal void CheckRoles() { foreach (var pin in shared.Pins) pin.CheckRole(); }
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
                    var pin = await entry.Database.PinAsync(token).ConfigureAwait(false); shared.Pins.Add(pin);
                    var pending = shared.Writers.FirstOrDefault(writer => ReferenceEquals(writer.Database, entry.Database)).Pending;
                    domains.Add(new(entry.Name, entry.Database, pending?.Before ?? pin.Snapshot, pending, entry.ReadOnly));
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
    internal async ValueTask ReleaseWriters()
    {
        lock (shared)
        {
            if (shared.WritersReleased) return;
            shared.WritersReleased = true;
        }
        Exception? failure = null;
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
        foreach (var pin in shared.Pins)
            try { await pin.DisposeAsync().ConfigureAwait(false); }
            catch (Exception error) { failure ??= error; }
        shared.Root.ReleaseAttachmentRegistry();
        if (failure is not null) System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(failure).Throw();
    }
}
