using System.Runtime.CompilerServices;
using System.Runtime.ExceptionServices;

namespace Jed;

// Pulling is permitted only after a site's upfront work has been charged.
// The window defers a source failure until all preceding rows have replayed.
internal static class HostPullWindow
{
    internal static async IAsyncEnumerable<SqlValue[]> Read(IAsyncEnumerable<SqlValue[]> source,
        Resolved[] roots, Meter meter, long firstUnit, Func<long?> remaining,
        [EnumeratorCancellation] CancellationToken token, int maximum = 64)
    {
        if (!HostBatchReplay.HasEligible(roots))
        {
            await foreach (var row in source.WithCancellation(token).ConfigureAwait(false)) yield return row;
            yield break;
        }
        int Capacity()
        {
            var count = Math.Min(64, maximum);
            if (remaining() is { } left) count = (int)Math.Min(count, Math.Max(0, left));
            if (firstUnit > 0 && meter.HostBatchHeadroom is { } budget && budget / firstUnit < count)
                count = Math.Min(count, (int)(budget / firstUnit) + 1);
            return count;
        }
        await using var iterator = source.GetAsyncEnumerator(token);
        var ended = false;
        while (!ended)
        {
            token.ThrowIfCancellationRequested();
            var capacity = Capacity();
            if (capacity == 0) yield break;
            var rows = new List<SqlValue[]>(capacity); Exception? failure = null;
            while (rows.Count < capacity)
            {
                bool moved;
                try { moved = await iterator.MoveNextAsync().ConfigureAwait(false); }
                catch (Exception error) { failure = error; break; }
                if (!moved) { ended = true; break; }
                rows.Add(iterator.Current);
                // A scan charges its whole page interval on the first pull.
                // Recompute headroom before reading additional stored rows.
                if (rows.Count == 1) capacity = Math.Max(1, Capacity());
            }
            using (meter.HostBatches.Begin(roots, rows))
                foreach (var row in rows) yield return row;
            if (failure is not null) ExceptionDispatchInfo.Capture(failure).Throw();
        }
    }
}

internal sealed class HostScanBatch : IDisposable
{
    private readonly HostBatchReplay owner;
    internal HostScanBatch? Previous { get; }
    internal object Identity { get; }
    internal Resolved[] Roots { get; }
    private readonly long? stop;
    private long passed;
    internal long? Remaining => stop is { } count ? Math.Max(0, count - passed) : null;
    internal HostScanBatch(HostBatchReplay owner, HostScanBatch? previous, SelectPlan plan)
    {
        this.owner = owner; Previous = previous; Identity = plan.HostBatchIdentity;
        Roots = (plan.Filter is { } filter ? new[] { filter } : []).Concat(plan.Projections.Select(item => item.Expression)).ToArray();
        stop = plan.Limit is { } limit ? Meter.Add(limit, plan.Offset) : null;
    }
    internal void Passed() { passed = Meter.Add(passed, 1); }
    public void Dispose() => owner.RestoreScan(Previous);
}
