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
        token.ThrowIfCancellationRequested();
        ObjectDisposedException.ThrowIf(disposed, this);
        var copy = new SqlValue[row.Length];
        for (var i = 0; i < row.Length; i++)
            copy[i] = row[i].IsDeferred ? await row[i].ResolveAsync(token).ConfigureAwait(false) : row[i];
        await AppendOwned(copy, token, logicalBytes).ConfigureAwait(false);
    }

    // The caller transfers a newly owned array containing only resident values.
    // Neither the caller nor any upstream cursor may mutate it after this call.
    internal ValueTask AppendOwned(SqlValue[] row, CancellationToken token, long? logicalBytes = null)
    {
        token.ThrowIfCancellationRequested();
        ObjectDisposedException.ThrowIf(disposed, this);
        if (types is not null && row.Length != types.Length) throw new InvalidOperationException("spool row type changed");
        for (var i = 0; i < row.Length; i++)
        {
            if (row[i].IsDeferred) throw new InvalidOperationException("owned spool row must be resident");
            if (types is not null && row[i].Type != types[i]) throw new InvalidOperationException("spool row type changed");
        }
        if (types is null)
        {
            types = new SqlType[row.Length];
            for (var i = 0; i < row.Length; i++) types[i] = row[i].Type;
        }
        if (file is not null) return AppendToFile(row, token, force: false);
        var bytes = logicalBytes ?? MemorySize.RowBytes(row);
        var admitted = meter.Memory.TryReserve(bytes);
        if (admitted) residentBytes = Meter.Add(residentBytes, bytes);
        if (!admitted || workMem > 0 && residentBytes > workMem) return AppendToFile(row, token, force: true);
        resident.Add(row);
        Count = checked(Count + 1);
        return ValueTask.CompletedTask;
    }

    private async ValueTask AppendToFile(SqlValue[] row, CancellationToken token, bool force)
    {
        if (force) await ForceDisk(token).ConfigureAwait(false);
        await Write(row, token).ConfigureAwait(false);
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
