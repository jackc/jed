namespace Jed;

internal sealed partial class StorageTree
{
    private sealed record PendingSize(byte[] Key, long Bytes, PendingSize? Left = null, PendingSize? Right = null)
    {
        internal int Height { get; } = 1 + Math.Max(Left?.Height ?? 0, Right?.Height ?? 0);
    }
    private PendingSize? pendingSizes;
    private bool trackPending;
    internal long PendingBytes { get; private set; }
    internal StorageTree WithPendingTracking(bool enabled)
    {
        if (trackPending == enabled) return this;
        return new(pager, schema, root, pageCount, retired) { pendingSizes = enabled ? pendingSizes : null, trackPending = enabled, PendingBytes = PendingBytes };
    }
    internal StorageTree CommittedResidentCopy() => new(pager, schema, root, pageCount, []);
    internal StorageTree ForgetPendingKeys() => new(pager, schema, root, pageCount, retired) { trackPending = trackPending, PendingBytes = PendingBytes };
    private StorageTree Written(StorageTree result, StorageRecord record)
    {
        long bytes = record.Key.Length;
        for (var i = 0; i < schema.Columns.Length; i++)
        {
            var column = schema.Columns[i]; if (column.KeyOffset >= 0) continue;
            bytes = Meter.Add(bytes, column.Width > 0 ? column.Width : record.Cells[i]?.Length ?? 0);
            if (column.Width == 0 && StorageLargeValues.External(record.Cells[i]))
                bytes = Meter.Add(bytes, StorageLargeValues.Inspect(record.Cells[i]!, pager.Encrypted).Stored);
        }
        result.trackPending = trackPending; result.pendingSizes = pendingSizes;
        result.PendingBytes = Meter.Add(PendingBytes, bytes);
        if (trackPending)
        {
            result.PendingBytes -= PendingSizeAt(pendingSizes, record.Key);
            result.pendingSizes = SetPendingSize(pendingSizes, record.Key, bytes);
        }
        return result;
    }
    private StorageTree Deleted(StorageTree result, ReadOnlySpan<byte> key)
    {
        result.trackPending = trackPending; result.pendingSizes = pendingSizes; result.PendingBytes = PendingBytes;
        if (trackPending)
        {
            result.PendingBytes -= PendingSizeAt(pendingSizes, key);
            result.pendingSizes = RemovePendingSize(pendingSizes, key);
        }
        return result;
    }
    private static long PendingSizeAt(PendingSize? node, ReadOnlySpan<byte> key)
    {
        while (node is not null)
        {
            var order = key.SequenceCompareTo(node.Key);
            if (order == 0) return node.Bytes;
            node = order < 0 ? node.Left : node.Right;
        }
        return 0;
    }
    private static PendingSize SetPendingSize(PendingSize? node, byte[] key, long bytes)
    {
        if (node is null) return new(key, bytes);
        var order = key.AsSpan().SequenceCompareTo(node.Key);
        return BalancePending(order < 0 ? new(node.Key, node.Bytes, SetPendingSize(node.Left, key, bytes), node.Right) :
            order > 0 ? new(node.Key, node.Bytes, node.Left, SetPendingSize(node.Right, key, bytes)) : new(node.Key, bytes, node.Left, node.Right));
    }
    private static PendingSize? RemovePendingSize(PendingSize? node, ReadOnlySpan<byte> key)
    {
        if (node is null) return null;
        var order = key.SequenceCompareTo(node.Key);
        if (order < 0) return BalancePending(new(node.Key, node.Bytes, RemovePendingSize(node.Left, key), node.Right));
        if (order > 0) return BalancePending(new(node.Key, node.Bytes, node.Left, RemovePendingSize(node.Right, key)));
        if (node.Left is null) return node.Right;
        if (node.Right is null) return node.Left;
        var next = node.Right; while (next.Left is not null) next = next.Left;
        return BalancePending(new(next.Key, next.Bytes, node.Left, RemovePendingSize(node.Right, next.Key)));
    }
    private static int PendingHeight(PendingSize? node) => node?.Height ?? 0;
    private static PendingSize BalancePending(PendingSize node)
    {
        if (PendingHeight(node.Left) > PendingHeight(node.Right) + 1)
        {
            var left = node.Left!;
            if (PendingHeight(left.Right) > PendingHeight(left.Left)) left = RotatePendingLeft(left);
            return RotatePendingRight(new(node.Key, node.Bytes, left, node.Right));
        }
        if (PendingHeight(node.Right) > PendingHeight(node.Left) + 1)
        {
            var right = node.Right!;
            if (PendingHeight(right.Left) > PendingHeight(right.Right)) right = RotatePendingRight(right);
            return RotatePendingLeft(new(node.Key, node.Bytes, node.Left, right));
        }
        return node;
    }
    private static PendingSize RotatePendingLeft(PendingSize node)
    {
        var pivot = node.Right!;
        return new(pivot.Key, pivot.Bytes, new(node.Key, node.Bytes, node.Left, pivot.Left), pivot.Right);
    }
    private static PendingSize RotatePendingRight(PendingSize node)
    {
        var pivot = node.Left!;
        return new(pivot.Key, pivot.Bytes, pivot.Left, new(node.Key, node.Bytes, pivot.Right, node.Right));
    }
}
