using System.Buffers.Binary;
using System.Runtime.CompilerServices;

namespace Jed;

// Mutable exact-key state with insertion-order replay. The disk directory has
// fixed metadata; a separate on-disk ordinal vector preserves first occurrence.
// Callers supply deterministic logical bytes, independent of the serialized form.
internal sealed class SpillKeyState(ScratchHost host, long workMem, Meter meter) : IAsyncDisposable
{
    internal sealed record Value(byte[] Bytes, long Ordinal);
    private sealed record Resident(byte[] Key, byte[] Bytes, long LogicalBytes, long Ordinal);
    private const int BucketCount = 64;
    private const int HeaderBytes = 32;
    private readonly Dictionary<string, Resident> resident = new(StringComparer.Ordinal);
    private IBlockStore? data;
    private IBlockStore? directory;
    private long length;
    private long held;
    private bool disposed;
    internal long Count { get; private set; }
    internal bool Spilled => data is not null;

    internal async ValueTask<Value?> Get(byte[] key, CancellationToken token)
    {
        if (data is null)
            return resident.TryGetValue(Convert.ToBase64String(key), out var item) ? new(item.Bytes.ToArray(), item.Ordinal) : null;
        var found = await Find(key, token).ConfigureAwait(false);
        return found is null ? null : await ReadValue(found.Value.Header, token).ConfigureAwait(false);
    }

    internal async ValueTask<long> Put(byte[] key, byte[] value, long logicalBytes, CancellationToken token)
    {
        if (logicalBytes < 0) throw new ArgumentOutOfRangeException(nameof(logicalBytes));
        if (data is null)
        {
            var identity = Convert.ToBase64String(key);
            resident.TryGetValue(identity, out var previous);
            var delta = logicalBytes - (previous?.LogicalBytes ?? 0);
            var admitted = delta <= 0 || meter.Memory.TryReserve(delta);
            if (admitted)
            {
                if (delta < 0) meter.Memory.Release(-delta);
                held += delta;
                if (workMem == 0 || held <= workMem)
                {
                    var ordinal = previous?.Ordinal ?? Count++;
                    resident[identity] = new(key.ToArray(), value.ToArray(), logicalBytes, ordinal);
                    return ordinal;
                }
            }
            await ForceDisk(token).ConfigureAwait(false);
        }
        return await PutDisk(key, value, token).ConfigureAwait(false);
    }

    internal async ValueTask ForceDisk(CancellationToken token)
    {
        if (data is not null) return;
        data = await host.CreateAsync(token).ConfigureAwait(false);
        directory = await host.CreateAsync(token).ConfigureAwait(false);
        await directory.WriteAsync(0, new byte[BucketCount * 8], token).ConfigureAwait(false);
        var expected = Count;
        Count = 0;
        foreach (var item in resident.Values.OrderBy(item => item.Ordinal))
            await PutDisk(item.Key, item.Bytes, token).ConfigureAwait(false);
        if (Count != expected) throw new InvalidOperationException("scratch state count changed");
        resident.Clear(); meter.Memory.Release(held); held = 0;
    }

    private static int Bucket(ReadOnlySpan<byte> key)
    {
        ulong hash = 14695981039346656037UL;
        foreach (var value in key) hash = unchecked((hash ^ value) * 1099511628211UL);
        return (int)(hash % BucketCount);
    }
    private async ValueTask<long> Pointer(long offset, CancellationToken token)
    {
        var bytes = new byte[8]; await directory!.ReadAsync(offset, bytes, token).ConfigureAwait(false);
        return BinaryPrimitives.ReadInt64BigEndian(bytes);
    }
    private async ValueTask SetPointer(long offset, long value, CancellationToken token)
    {
        var bytes = new byte[8]; BinaryPrimitives.WriteInt64BigEndian(bytes, value);
        await directory!.WriteAsync(offset, bytes, token).ConfigureAwait(false);
    }
    private async ValueTask<(long Offset, byte[] Header)?> Find(byte[] key, CancellationToken token)
    {
        var pointer = await Pointer(Bucket(key) * 8L, token).ConfigureAwait(false);
        while (pointer != 0)
        {
            var offset = pointer - 1; var header = new byte[HeaderBytes];
            await data!.ReadAsync(offset, header, token).ConfigureAwait(false);
            var size = BinaryPrimitives.ReadInt32BigEndian(header.AsSpan(28));
            if (size == key.Length)
            {
                var stored = new byte[size]; await data.ReadAsync(offset + HeaderBytes, stored, token).ConfigureAwait(false);
                if (stored.AsSpan().SequenceEqual(key)) return (offset, header);
            }
            pointer = BinaryPrimitives.ReadInt64BigEndian(header);
        }
        return null;
    }
    private async ValueTask<Value> ReadValue(byte[] header, CancellationToken token)
    {
        var offset = BinaryPrimitives.ReadInt64BigEndian(header.AsSpan(16));
        var count = BinaryPrimitives.ReadInt32BigEndian(header.AsSpan(24));
        var bytes = new byte[count]; await data!.ReadAsync(offset, bytes, token).ConfigureAwait(false);
        return new(bytes, BinaryPrimitives.ReadInt64BigEndian(header.AsSpan(8)));
    }
    private async ValueTask<long> PutDisk(byte[] key, byte[] value, CancellationToken token)
    {
        var previous = await Find(key, token).ConfigureAwait(false);
        if (previous is { } existing)
        {
            await data!.WriteAsync(length, value, token).ConfigureAwait(false);
            BinaryPrimitives.WriteInt64BigEndian(existing.Header.AsSpan(16), length);
            BinaryPrimitives.WriteInt32BigEndian(existing.Header.AsSpan(24), value.Length);
            length = checked(length + value.Length);
            await data.WriteAsync(existing.Offset, existing.Header, token).ConfigureAwait(false);
            return BinaryPrimitives.ReadInt64BigEndian(existing.Header.AsSpan(8));
        }
        var ordinal = Count; var offset = length; var header = new byte[HeaderBytes];
        var bucket = Bucket(key) * 8L;
        var head = await Pointer(bucket, token).ConfigureAwait(false);
        BinaryPrimitives.WriteInt64BigEndian(header, head);
        BinaryPrimitives.WriteInt64BigEndian(header.AsSpan(8), ordinal);
        BinaryPrimitives.WriteInt64BigEndian(header.AsSpan(16), checked(offset + HeaderBytes + key.Length));
        BinaryPrimitives.WriteInt32BigEndian(header.AsSpan(24), value.Length);
        BinaryPrimitives.WriteInt32BigEndian(header.AsSpan(28), key.Length);
        await data!.WriteAsync(offset, header, token).ConfigureAwait(false);
        await data.WriteAsync(offset + HeaderBytes, key, token).ConfigureAwait(false);
        await data.WriteAsync(offset + HeaderBytes + key.Length, value, token).ConfigureAwait(false);
        length = checked(offset + HeaderBytes + key.Length + value.Length);
        await SetPointer(bucket, offset + 1, token).ConfigureAwait(false);
        await SetPointer(checked(BucketCount * 8L + ordinal * 8), offset + 1, token).ConfigureAwait(false);
        Count = checked(Count + 1); return ordinal;
    }
    internal async IAsyncEnumerable<(byte[] Key, Value Value)> Read([EnumeratorCancellation] CancellationToken token)
    {
        if (data is null)
        {
            foreach (var item in resident.Values.OrderBy(item => item.Ordinal))
            { token.ThrowIfCancellationRequested(); yield return (item.Key.ToArray(), new(item.Bytes.ToArray(), item.Ordinal)); }
            yield break;
        }
        for (long ordinal = 0; ordinal < Count; ordinal++)
        {
            var offset = await Pointer(checked(BucketCount * 8L + ordinal * 8), token).ConfigureAwait(false) - 1;
            var header = new byte[HeaderBytes]; await data.ReadAsync(offset, header, token).ConfigureAwait(false);
            var key = new byte[BinaryPrimitives.ReadInt32BigEndian(header.AsSpan(28))];
            await data.ReadAsync(offset + HeaderBytes, key, token).ConfigureAwait(false);
            yield return (key, await ReadValue(header, token).ConfigureAwait(false));
        }
    }
    public async ValueTask DisposeAsync()
    {
        if (disposed) return; disposed = true;
        resident.Clear(); meter.Memory.Release(held); held = 0;
        try { if (data is not null) await data.DisposeAsync().ConfigureAwait(false); }
        finally { if (directory is not null) await directory.DisposeAsync().ConfigureAwait(false); }
    }
}
