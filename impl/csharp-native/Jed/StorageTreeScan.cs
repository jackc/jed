using System.Runtime.CompilerServices;

namespace Jed;

internal sealed partial class StorageTree
{
    internal async ValueTask<long> CountRangePagesAsync(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        CancellationToken cancellationToken = default)
    {
        if (root is null || EmptyRange(lower, lowerInclusive, upper, upperInclusive)) return 0;
        return await CountRange(root, lower, lowerInclusive, upper, upperInclusive, new HashSet<uint>(), 0, cancellationToken).ConfigureAwait(false);
    }
    private async ValueTask<long> CountRange(Ref reference, byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        HashSet<uint> path, int depth, CancellationToken cancellationToken)
    {
        CheckPath(reference, path, depth);
        try
        {
            var node = await Load(reference, cancellationToken).ConfigureAwait(false);
            long count = 1;
            if (!node.IsLeaf)
                for (var i = 0; i < node.Children!.Length; i++)
                    if (Intersects(node, i, lower, upper, upperInclusive))
                        count += await CountRange(node.Children[i], lower, lowerInclusive, upper, upperInclusive, path, depth + 1, cancellationToken).ConfigureAwait(false);
            return count;
        }
        finally { if (reference.Page != 0) path.Remove(reference.Page); }
    }
    internal async IAsyncEnumerable<StorageRecord> ScanRangeAsync(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        bool reverse = false, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        if (root is null || EmptyRange(lower, lowerInclusive, upper, upperInclusive)) yield break;
        await foreach (var row in WalkRange(root, lower, lowerInclusive, upper, upperInclusive, reverse, new HashSet<uint>(), 0, cancellationToken).ConfigureAwait(false)) yield return row;
    }
    private async IAsyncEnumerable<StorageRecord> WalkRange(Ref reference, byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        bool reverse, HashSet<uint> path, int depth, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        CheckPath(reference, path, depth);
        try
        {
            var node = await Load(reference, cancellationToken).ConfigureAwait(false);
            if (node.IsLeaf)
            {
                var from = lower is null ? 0 : LowerBound(node, lower);
                if (lower is not null && !lowerInclusive && from < node.Count && node.Key(from).AsSpan().SequenceEqual(lower)) from++;
                var until = upper is null ? node.Count : LowerBound(node, upper);
                if (upper is not null && upperInclusive && until < node.Count && node.Key(until).AsSpan().SequenceEqual(upper)) until++;
                for (var i = reverse ? until - 1 : from; reverse ? i >= from : i < until; i += reverse ? -1 : 1)
                {
                    cancellationToken.ThrowIfCancellationRequested(); yield return Clone(node.Record(i));
                }
            }
            else
            {
                for (var i = reverse ? node.Children!.Length - 1 : 0; reverse ? i >= 0 : i < node.Children!.Length; i += reverse ? -1 : 1)
                    if (Intersects(node, i, lower, upper, upperInclusive))
                        await foreach (var row in WalkRange(node.Children![i], lower, lowerInclusive, upper, upperInclusive, reverse, path, depth + 1, cancellationToken).ConfigureAwait(false)) yield return row;
            }
        }
        finally { if (reference.Page != 0) path.Remove(reference.Page); }
    }
    private static bool EmptyRange(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive)
    {
        if (lower is null || upper is null) return false;
        var order = lower.AsSpan().SequenceCompareTo(upper);
        return order > 0 || order == 0 && !(lowerInclusive && upperInclusive);
    }
    private static bool Intersects(Node node, int child, byte[]? lower, byte[]? upper, bool upperInclusive)
    {
        if (lower is not null && child < node.Keys!.Length && lower.AsSpan().SequenceCompareTo(node.Keys[child]) >= 0) return false;
        if (upper is not null && child > 0)
        {
            var compare = upper.AsSpan().SequenceCompareTo(node.Keys![child - 1]);
            if (compare < 0 || compare == 0 && !upperInclusive) return false;
        }
        return true;
    }
}
