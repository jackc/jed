using System.Buffers.Binary;
using System.Runtime.CompilerServices;

namespace Jed;

// Full-hash buckets preserve insertion order. Large and skewed buckets replay
// one record at a time; their linked lists and directory both live in scratch.
internal sealed class SpillHashRows(ScratchHost host, long workMem, Meter meter) : IAsyncDisposable
{
    private readonly Dictionary<ulong, List<SqlValue[]>> resident = [];
    private SqlType[]? types;
    private SpillKeyState? heads;
    private IBlockStore? rows;
    private long end;
    private long held;
    internal bool Spilled => rows is not null;
    internal async ValueTask Add(ulong hash, SqlValue[] row, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        types ??= row.Select(value => value.Type).ToArray();
        if (rows is null)
        {
            var bytes = Meter.Add(MemorySchedule.Entry, MemorySize.RowBytes(row));
            var admitted = meter.Memory.TryReserve(bytes);
            if (admitted) held = Meter.Add(held, bytes);
            if (admitted && (workMem == 0 || held <= workMem))
            {
                if (!resident.TryGetValue(hash, out var bucket)) resident.Add(hash, bucket = []);
                bucket.Add(row); return;
            }
            rows = await host.CreateAsync(token).ConfigureAwait(false);
            heads = new(host, workMem, meter);
            await heads.ForceDisk(token).ConfigureAwait(false);
            foreach (var bucket in resident.OrderBy(entry => entry.Key))
                foreach (var entry in bucket.Value) await Append(bucket.Key, entry, token).ConfigureAwait(false);
            resident.Clear(); meter.Memory.Release(held); held = 0;
        }
        await Append(hash, row, token).ConfigureAwait(false);
    }
    private static byte[] HashKey(ulong hash)
    { var key = new byte[8]; BinaryPrimitives.WriteUInt64BigEndian(key, hash); return key; }
    private static byte[] Encode(SqlValue[] row) => ScratchValues.Encode(row);
    private SqlValue[] Decode(byte[] bytes) => ScratchValues.Decode(bytes, types!);
    private async ValueTask Append(ulong hash, SqlValue[] row, CancellationToken token)
    {
        var key = HashKey(hash); var prior = await heads!.Get(key, token).ConfigureAwait(false);
        var bounds = prior?.Bytes ?? new byte[16];
        var bytes = Encode(row); var at = end; var header = new byte[12];
        BinaryPrimitives.WriteInt32BigEndian(header.AsSpan(8), bytes.Length);
        await rows!.WriteAsync(at, header, token).ConfigureAwait(false);
        await rows.WriteAsync(at + 12, bytes, token).ConfigureAwait(false);
        end = checked(at + 12 + bytes.Length);
        var pointer = new byte[8]; BinaryPrimitives.WriteInt64BigEndian(pointer, at + 1);
        if (prior is null) BinaryPrimitives.WriteInt64BigEndian(bounds, at + 1);
        else await rows.WriteAsync(BinaryPrimitives.ReadInt64BigEndian(bounds.AsSpan(8)) - 1, pointer, token).ConfigureAwait(false);
        BinaryPrimitives.WriteInt64BigEndian(bounds.AsSpan(8), at + 1);
        await heads.Put(key, bounds, 0, token).ConfigureAwait(false);
    }
    internal async IAsyncEnumerable<SqlValue[]> Read(ulong hash, [EnumeratorCancellation] CancellationToken token)
    {
        if (rows is null)
        {
            if (resident.TryGetValue(hash, out var bucket)) foreach (var row in bucket)
            { token.ThrowIfCancellationRequested(); yield return row; }
            yield break;
        }
        var bounds = await heads!.Get(HashKey(hash), token).ConfigureAwait(false);
        if (bounds is null) yield break;
        var pointer = BinaryPrimitives.ReadInt64BigEndian(bounds.Bytes);
        while (pointer != 0)
        {
            var at = pointer - 1; var header = new byte[12];
            await rows.ReadAsync(at, header, token).ConfigureAwait(false);
            var bytes = new byte[BinaryPrimitives.ReadInt32BigEndian(header.AsSpan(8))];
            await rows.ReadAsync(at + 12, bytes, token).ConfigureAwait(false);
            pointer = BinaryPrimitives.ReadInt64BigEndian(header);
            yield return Decode(bytes);
        }
    }
    public async ValueTask DisposeAsync()
    {
        resident.Clear(); meter.Memory.Release(held); held = 0;
        try { if (rows is not null) await rows.DisposeAsync().ConfigureAwait(false); }
        finally { if (heads is not null) await heads.DisposeAsync().ConfigureAwait(false); }
    }
}
