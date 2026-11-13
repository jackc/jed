namespace Jed;

// A read-only view of a serialized but unpublished generation. Reclamation can
// walk it before storage admission without writing or adopting any page.
internal sealed class StoragePlannedView(IBlockStore source, int pageSize, uint pageCount,
    IReadOnlyDictionary<uint, byte[]> pages) : IBlockStore
{
    public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        if (offset < 0 || offset % pageSize != 0 || destination.Length != pageSize || offset / pageSize >= pageCount)
            throw new ArgumentException("planned page read is outside its generation");
        if (!pages.TryGetValue((uint)(offset / pageSize), out var bytes)) return source.ReadAsync(offset, destination, cancellationToken);
        bytes.AsMemory().CopyTo(destination); return ValueTask.CompletedTask;
    }
    public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default) => ValueTask.FromResult((long)pageCount * pageSize);
    public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default) => throw new InvalidOperationException("planned view is read-only");
    public ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default) => throw new InvalidOperationException("planned view is read-only");
    public ValueTask SyncAsync(CancellationToken cancellationToken = default) => throw new InvalidOperationException("planned view has no durability");
    public ValueTask DisposeAsync() => ValueTask.CompletedTask;
}
