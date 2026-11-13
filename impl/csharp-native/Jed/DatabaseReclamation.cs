namespace Jed;

public sealed partial class Database
{
    // Called only by the page owner after acknowledging a client commit, while
    // retaining that client's writer lease. No other writer can interleave this
    // housekeeping generation with the client generation that made it due.
    private async ValueTask RunRemoteMaintenanceAsync(long role, CancellationToken token)
    {
        CheckRole(role);
        var before = committer.Current;
        if (before.ReclamationLive != 0 && ((long)before.Meta.PageCount - before.FreePages.Count <= ReclamationFacts.CompactMinPages ||
            (long)before.Meta.PageCount - before.FreePages.Count <= ReclamationFacts.CompactGrowth * before.ReclamationLive))
            return;
        lock (gate)
        {
            var oldest = pins.Count == 0 ? ulong.MaxValue : pins.Keys.Min();
            if (fileState!.Ledger!.OldestPin is { } remoteOldest) oldest = Math.Min(oldest, remoteOldest);
            if (oldest < before.Meta.TransactionId) return;
        }
        var snapshot = await OwnerSnapshotAsync(token).ConfigureAwait(false);
        var pending = new PendingDatabase(snapshot) { Changed = true, Role = role, StorageBefore = before };
        await CommitAsync(pending, token).ConfigureAwait(false);
    }
}
