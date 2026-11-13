namespace Jed;

// A pager belongs to a pinned remote snapshot. Its read version never changes
// when another session adopts a newer meta or drops its last pin.
internal sealed class PageClientStore(PageClientConnection connection, PageReadSnapshot snapshot) : IBlockStore
{
    private int disposed;
    private int PageSize => snapshot.Meta.Length;
    internal PageClientConnection Connection => connection;
    public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
    {
        ObjectDisposedException.ThrowIf(Volatile.Read(ref disposed) != 0, this);
        MemoryBlockStore.CheckRange(offset, destination.Length);
        while (!destination.IsEmpty)
        {
            var index = offset / PageSize;
            if (index > uint.MaxValue) throw StorageFormat.Corrupt("remote page offset out of range");
            var within = (int)(offset % PageSize); var count = Math.Min(PageSize - within, destination.Length);
            var page = index < 2 ? snapshot.Meta : (await connection.ReadPagesAsync(snapshot.TransactionId, [(uint)index], cancellationToken).ConfigureAwait(false))[0];
            page.AsMemory(within, count).CopyTo(destination); destination = destination[count..]; offset += count;
        }
    }
    public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return ValueTask.FromResult((long)System.Buffers.Binary.BinaryPrimitives.ReadUInt32BigEndian(snapshot.Meta.AsSpan(24)) * PageSize);
    }
    public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default) => throw new InvalidOperationException("remote pages are written only by a validated commit delta");
    public ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default) => throw new InvalidOperationException("remote file size belongs to the owner");
    public ValueTask SyncAsync(CancellationToken cancellationToken = default) => throw new InvalidOperationException("remote durability belongs to the owner");
    public ValueTask DisposeAsync() { Interlocked.Exchange(ref disposed, 1); return ValueTask.CompletedTask; }
}
