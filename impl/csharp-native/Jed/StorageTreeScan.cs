using System.Runtime.CompilerServices;

namespace Jed;

internal sealed partial class StorageTree
{
    internal async ValueTask<long> CountRangePagesAsync(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        CancellationToken cancellationToken = default)
    {
        if (root is null || EmptyRange(lower, lowerInclusive, upper, upperInclusive)) return 0;
        cancellationToken.ThrowIfCancellationRequested();
        if (HasShape(root)) return CountRangeNodes(lower, lowerInclusive, upper, upperInclusive);
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
    internal readonly record struct ScanRow(StorageLeaf? Leaf, StorageRecord? Resident, int Index)
    {
        internal StorageRecord Record() => Leaf is null ? Clone(Resident!) : Leaf.Record(Index);
        internal SqlValue[] DecodeFixed(CatalogTable table, StorageTree tree) => Leaf is null
            ? tree.DecodeDeferredRow(table, Resident!) : Leaf.FixedRow(Index, table);
    }
    // A chunk retains only one immutable leaf, valid across subsequent awaits.
    // Its indices are leaf-local; callers own any reusable decode destination.
    internal readonly record struct ScanChunk(StorageLeaf? Leaf, StorageRecord[]? Resident, int From, int Until, bool Reverse)
    {
        internal ScanRow Row(int index)
        {
            if (index < From || index >= Until) throw new ArgumentOutOfRangeException(nameof(index));
            return new(Leaf, Resident?[index], index);
        }
        internal void DecodeFixedInto(CatalogTable table, StorageTree tree, int index, SqlValue[] destination)
        {
            if (index < From || index >= Until) throw new ArgumentOutOfRangeException(nameof(index));
            if (Leaf is not null) Leaf.FixedRowInto(index, table, destination);
            else tree.DecodeDeferredRowInto(table, Resident![index], destination);
        }
    }
    internal async IAsyncEnumerable<StorageRecord> ScanRangeAsync(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        bool reverse = false, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        await foreach (var row in ScanRangeRowsAsync(lower, lowerInclusive, upper, upperInclusive, reverse, cancellationToken).ConfigureAwait(false))
            yield return row.Record();
    }
    internal async IAsyncEnumerable<ScanRow> ScanRangeRowsAsync(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        bool reverse = false, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        if (root is null || EmptyRange(lower, lowerInclusive, upper, upperInclusive)) yield break;
        if (lower is not null && upper is not null && lowerInclusive && upperInclusive && lower.AsSpan().SequenceEqual(upper))
        {
            if (await FindRowAsync(lower, cancellationToken).ConfigureAwait(false) is { } found)
            { cancellationToken.ThrowIfCancellationRequested(); yield return found; }
            yield break;
        }
        await foreach (var chunk in ScanRangeChunksAsync(lower, lowerInclusive, upper, upperInclusive, reverse, cancellationToken).ConfigureAwait(false))
            for (var i = reverse ? chunk.Until - 1 : chunk.From; reverse ? i >= chunk.From : i < chunk.Until; i += reverse ? -1 : 1)
            {
                cancellationToken.ThrowIfCancellationRequested();
                yield return chunk.Row(i);
            }
    }
    internal async IAsyncEnumerable<ScanChunk> ScanRangeChunksAsync(byte[]? lower, bool lowerInclusive, byte[]? upper, bool upperInclusive,
        bool reverse = false, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        if (root is null || EmptyRange(lower, lowerInclusive, upper, upperInclusive)) yield break;
        // Only page transitions need asynchronous work. Keep one iterator for
        // the entire traversal instead of forwarding every row through an
        // async iterator at every interior level.
        var path = new HashSet<uint>();
        var parents = new Stack<(Ref Reference, Node Node, int Next)>();
        Ref? reference = root;
        var step = reverse ? -1 : 1;
        while (reference is not null)
        {
            cancellationToken.ThrowIfCancellationRequested();
            CheckPath(reference, path, parents.Count);
            var node = await Load(reference, cancellationToken).ConfigureAwait(false);
            if (node.IsLeaf)
            {
                var from = lower is null ? 0 : LowerBound(node, lower);
                if (lower is not null && !lowerInclusive && from < node.Count && node.CompareKey(from, lower) == 0) from++;
                var until = upper is null ? node.Count : LowerBound(node, upper);
                if (upper is not null && upperInclusive && until < node.Count && node.CompareKey(until, upper) == 0) until++;
                if (from < until)
                {
                    cancellationToken.ThrowIfCancellationRequested();
                    yield return new(node.Packed, node.Rows, from, until, reverse);
                }
            }
            else
            {
                var child = NextChild(node, reverse ? node.Children!.Length - 1 : 0);
                if (child >= 0 && child < node.Children!.Length)
                {
                    parents.Push((reference, node, child + step));
                    reference = node.Children[child];
                    continue;
                }
            }
            if (reference.Page != 0) path.Remove(reference.Page);
            reference = null;
            while (parents.TryPop(out var parent))
            {
                var child = NextChild(parent.Node, parent.Next);
                if (child >= 0 && child < parent.Node.Children!.Length)
                {
                    parents.Push((parent.Reference, parent.Node, child + step));
                    reference = parent.Node.Children[child];
                    break;
                }
                if (parent.Reference.Page != 0) path.Remove(parent.Reference.Page);
            }
        }
        int NextChild(Node node, int index)
        {
            while (index >= 0 && index < node.Children!.Length && !Intersects(node, index, lower, upper, upperInclusive)) index += step;
            return index;
        }
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
