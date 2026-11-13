using System.Buffers.Binary;
using System.Runtime.CompilerServices;

namespace Jed;

// Repeatable source-order rows. Only the resident prefix is charged; scratch
// reads are unmetered and callers own any retained read heads or output rows.
internal sealed class SpillRowSpool(ScratchHost host, long workMem, Meter meter) : IAsyncDisposable
{
    private readonly List<SqlValue[]> resident = [];
    private SqlType[]? types;
    private IBlockStore? file;
    private long length;
    private long residentBytes;
    private bool disposed;
    internal long Count { get; private set; }
    internal long ResidentBytes => residentBytes;
    internal bool Spilled => file is not null;

    internal async ValueTask Append(SqlValue[] row, CancellationToken token, long? logicalBytes = null)
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        var copy = new SqlValue[row.Length];
        for (var i = 0; i < row.Length; i++) copy[i] = await row[i].ResolveAsync(token).ConfigureAwait(false);
        types ??= copy.Select(value => value.Type).ToArray();
        if (copy.Length != types.Length || copy.Where((value, slot) => value.Type != types[slot]).Any())
            throw new InvalidOperationException("spool row type changed");
        if (file is not null) await Write(copy, token).ConfigureAwait(false);
        else
        {
            var bytes = logicalBytes ?? MemorySize.RowBytes(copy);
            var admitted = meter.Memory.TryReserve(bytes);
            if (admitted) residentBytes = Meter.Add(residentBytes, bytes);
            if (!admitted || workMem > 0 && residentBytes > workMem)
            {
                await ForceDisk(token).ConfigureAwait(false);
                await Write(copy, token).ConfigureAwait(false);
            }
            else resident.Add(copy);
        }
        Count = checked(Count + 1);
    }
    internal async ValueTask ForceDisk(CancellationToken token)
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (file is not null) return;
        file = await host.CreateAsync(token).ConfigureAwait(false);
        foreach (var row in resident) await Write(row, token).ConfigureAwait(false);
        resident.Clear(); meter.Memory.Release(residentBytes); residentBytes = 0;
    }
    private async ValueTask Write(SqlValue[] row, CancellationToken token)
    {
        var bytes = ScratchValues.Encode(row); var header = new byte[4]; BinaryPrimitives.WriteUInt32BigEndian(header, checked((uint)bytes.Length));
        await file!.WriteAsync(length, header, token).ConfigureAwait(false); length = checked(length + 4);
        await file.WriteAsync(length, bytes, token).ConfigureAwait(false); length = checked(length + bytes.Length);
    }
    internal async IAsyncEnumerable<SqlValue[]> Read([EnumeratorCancellation] CancellationToken token = default)
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (file is null) { foreach (var row in resident) { token.ThrowIfCancellationRequested(); yield return row; } yield break; }
        long offset = 0; var header = new byte[4];
        while (offset < length)
        {
            await file.ReadAsync(offset, header, token).ConfigureAwait(false); offset += 4;
            var count = BinaryPrimitives.ReadUInt32BigEndian(header);
            if (count > int.MaxValue || count > length - offset) throw StorageFormat.Corrupt("invalid scratch row framing");
            var bytes = new byte[(int)count]; await file.ReadAsync(offset, bytes, token).ConfigureAwait(false); offset += count;
            yield return Decode(bytes);
        }
    }
    private SqlValue[] Decode(byte[] bytes) => ScratchValues.Decode(bytes, types!);
    public async ValueTask DisposeAsync()
    {
        if (disposed) return; disposed = true;
        resident.Clear(); meter.Memory.Release(residentBytes); residentBytes = 0;
        if (file is not null) await file.DisposeAsync().ConfigureAwait(false);
    }
}
