namespace Jed;

// Runtime-only facts. Every array is a sorted snapshot and is never mutated after
// publication; recovering a file starts at its selected candidate with no batch.
internal sealed record StorageBatch(ulong FloorTransactionId, uint[] FloorFree,
    StorageManifestEntry[] Pending, long? DeadlineMilliseconds, bool ShrinkAfterFlush = false)
{
    internal static readonly IComparer<StorageManifestEntry> PageComparer =
        Comparer<StorageManifestEntry>.Create((left, right) => left.Page.CompareTo(right.Page));
}

internal sealed partial class StorageCommitter
{
    // The writer gate is held by the caller. Reading the injected clock belongs
    // to the host, which can ensure one reading also serves a deadline rollover.
    internal async ValueTask<long?> BeginPlanAsync(bool synchronous, long? nowMilliseconds,
        long flushIntervalMilliseconds, bool shrink = true, CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested();
        if (IsPoisoned) throw new SqlException(SqlStates.IoError, "storage handle failed; close and reopen");
        ArgumentOutOfRangeException.ThrowIfNegative(flushIntervalMilliseconds);
        if (Current.Batch is { DeadlineMilliseconds: { } deadline } && nowMilliseconds is { } now && now >= deadline)
            await FlushAsync(shrink, token).ConfigureAwait(false);
        if (Current.Batch is { } batch) return batch.DeadlineMilliseconds;
        if (synchronous || flushIntervalMilliseconds == 0 || nowMilliseconds is null) return null;
        return nowMilliseconds > long.MaxValue - flushIntervalMilliseconds ? long.MaxValue : nowMilliseconds + flushIntervalMilliseconds;
    }

    internal async ValueTask FlushAsync(bool shrink = true, CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested();
        if (IsPoisoned) throw new SqlException(SqlStates.IoError, "storage handle failed; close and reopen");
        var before = Current;
        if (before.Batch is not { } batch) return;
        try
        {
            await pager.Store.SyncAsync(token).ConfigureAwait(false);
            Current = before with { FloorSlot = before.Slot, Batch = null };
            stabilize = false;
        }
        catch { IsPoisoned = true; pager.Revoke(); throw; }
        if (shrink && batch.ShrinkAfterFlush) await ShrinkAsync(before.Meta.PageCount).ConfigureAwait(false);
    }

    private async ValueTask ShrinkAsync(uint pageCount)
    {
        // Only remove slack after a successful barrier. A failed best-effort
        // shrink cannot undo durability or change the acknowledged outcome.
        try
        {
            var floor = Math.Max(1, 16 * 1024 / pager.PageSize);
            var cap = Math.Max(1, 1024 * 1024 / pager.PageSize);
            var target = checked(((long)pageCount + Math.Clamp((long)pageCount, floor, cap)) * pager.PageSize);
            if (target < await pager.Store.GetLengthAsync(CancellationToken.None).ConfigureAwait(false))
                await pager.Store.SetLengthAsync(target, CancellationToken.None).ConfigureAwait(false);
        }
        catch (Exception error) when (error is IOException or SqlException) { }
    }
}
