namespace Jed;

// Runtime-only facts. Every array is a sorted snapshot and is never mutated after
// publication; recovering a file starts at its selected candidate with no batch.
internal sealed record StorageBatch(ulong FloorTransactionId, uint[] FloorFree,
    StorageManifestEntry[] Pending, long? DeadlineMicroseconds, bool ShrinkAfterFlush = false)
{
    internal static readonly IComparer<StorageManifestEntry> PageComparer =
        Comparer<StorageManifestEntry>.Create((left, right) => left.Page.CompareTo(right.Page));
}

internal sealed partial class StorageCommitter
{
    private bool shrinkDeferred;
    internal async ValueTask PublishClientAsync(StorageRecovered recovered, StorageManifestEntry[] cumulativeEntries,
        bool synchronous, long? deadlineMicroseconds, CancellationToken token = default)
    {
        if (IsPoisoned) throw new SqlException(SqlStates.IoError, "storage handle failed; close and reopen");
        var before = Current;
        try
        {
            if (synchronous) await pager.Store.SyncAsync(token).ConfigureAwait(false);
            var batch = synchronous ? null : new StorageBatch(
                before.Batch?.FloorTransactionId ?? before.Meta.TransactionId,
                before.Batch?.FloorFree ?? before.FreePages.ToArray(), cumulativeEntries.OrderBy(entry => entry.Page).ToArray(),
                before.Batch?.DeadlineMicroseconds ?? deadlineMicroseconds, before.Batch?.ShrinkAfterFlush == true);
            Current = recovered with { FloorSlot = synchronous ? recovered.Slot : before.FloorSlot, Batch = batch };
            stabilize = false;
        }
        catch { IsPoisoned = true; pager.Revoke(); throw; }
        var cap = pager.Encrypted ? StorageCryptoMeta.InlineCapacity(pager.PageSize) : StorageFormat.InlineCapacity(pager.PageSize);
        if (Current.Batch is { } pending && pending.Pending.Length >= cap)
        {
            try { await FlushAsync(shrink: false, token: CancellationToken.None).ConfigureAwait(false); }
            catch (Exception error) when (error is IOException or SqlException or OperationCanceledException) { }
        }
    }

    // The writer gate is held by the caller. Reading the injected clock belongs
    // to the host, which can ensure one reading also serves a deadline rollover.
    internal async ValueTask<long?> BeginPlanAsync(bool synchronous, long? nowMicroseconds,
        long flushIntervalMilliseconds, bool shrink = true, CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested();
        if (IsPoisoned) throw new SqlException(SqlStates.IoError, "storage handle failed; close and reopen");
        ArgumentOutOfRangeException.ThrowIfNegative(flushIntervalMilliseconds);
        if (Current.Batch is { DeadlineMicroseconds: { } deadline } && nowMicroseconds is { } now && now >= deadline)
            await FlushAsync(shrink, token).ConfigureAwait(false);
        if (Current.Batch is { } batch) return batch.DeadlineMicroseconds;
        if (synchronous || flushIntervalMilliseconds == 0 || nowMicroseconds is null) return null;
        var next = (Int128)nowMicroseconds.Value + (Int128)flushIntervalMilliseconds * 1000;
        return next > long.MaxValue ? long.MaxValue : (long)next;
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
        if (batch.ShrinkAfterFlush)
        {
            if (shrink) { await ShrinkAsync(before.Meta.PageCount).ConfigureAwait(false); shrinkDeferred = false; }
            else shrinkDeferred = true;
        }
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
