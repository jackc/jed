namespace Jed;

public sealed partial class Database
{
    /// <summary>Set the committed-storage allowance for main or a named attachment; zero disables it.</summary>
    public void SetStorageLimit(long maximumBytes, string database = "main")
    {
        ArgumentOutOfRangeException.ThrowIfNegative(maximumBytes); ArgumentNullException.ThrowIfNull(database);
        lock (gate)
        {
            Check();
            if (database == "main") MaxStorageBytes = maximumBytes;
            else if (attachments.TryGetValue(database, out var attached)) attached.Database.SetStorageLimit(maximumBytes);
            else throw new SqlException(SqlStates.UndefinedObject, "database is not attached");
        }
    }

    // Reclaims identities without moving a page or changing the published version.
    // The writer gate excludes another publication; a new reader can only pin the
    // current version, whose entire reachable set is retained by this walk.
    private async ValueTask<bool> ReclaimMemoryOrphansAsync(PendingDatabase pending, CancellationToken token)
    {
        DatabaseSnapshot snapshot; StorageRecovered before;
        lock (gate)
        {
            Check();
            if (fileState is not null || pins.Keys.Any(version => version < current.Meta.TransactionId)) return false;
            snapshot = current with { Pager = pager }; before = committer.Current;
        }
        var keep = await new StorageSnapshotCopy(pager, snapshot).ReachableAsync(token).ConfigureAwait(false);
        keep.UnionWith(before.ProtectedPages); keep.UnionWith(before.ManifestPages); keep.UnionWith(before.FreeListPages);
        var free = new SortedSet<uint>(before.FreePages);
        for (uint page = pager.Encrypted ? 4U : 2U; page < before.Meta.PageCount; page++)
        {
            if ((page & 1023) == 0) token.ThrowIfCancellationRequested();
            if (!keep.Contains(page)) free.Add(page);
        }
        if (free.Count == before.FreePages.Count) return false;
        lock (gate)
        {
            Check();
            if (!ReferenceEquals(before, committer.Current)) throw new InvalidOperationException("memory reclamation lost its writer snapshot");
            var reclaimed = before with { FreePages = free };
            committer.Adopt(reclaimed, stabilized: true);
            pending.StorageBefore = reclaimed;
            foreach (var page in free) if (!before.FreePages.Contains(page)) retiredAt.Remove(page);
        }
        return true;
    }
}

public sealed partial class Session
{
    internal async ValueTask SetDatabaseStorageLimitAsync(long maximumBytes, string name)
    {
        using var operation = Enter();
        var database = await GetDatabase(CancellationToken.None).ConfigureAwait(false);
        database.SetStorageLimit(maximumBytes, name);
    }
}
