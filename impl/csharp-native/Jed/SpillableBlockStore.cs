using System.Security.Cryptography;

namespace Jed;

// Physical residency only. Operators own logical work_mem/query-memory charges.
internal sealed class SpillableBlockStore : IBlockStore
{
    private readonly ScratchHost host;
    private readonly long threshold;
    private IBlockStore store = new MemoryBlockStore();
    private readonly SemaphoreSlim gate = new(1, 1);
    private bool disposed;
    internal bool Spilled { get; private set; }
    internal SpillableBlockStore(ScratchHost host, long residentBytes)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(residentBytes); this.host = host; threshold = residentBytes;
    }
    private async ValueTask Grow(long requested, CancellationToken token)
    {
        if (Spilled || requested <= threshold) return;
        var replacement = await host.CreateAsync(token).ConfigureAwait(false); var installed = false;
        var buffer = new byte[65536];
        try
        {
            var length = await store.GetLengthAsync(token).ConfigureAwait(false);
            for (long offset = 0; offset < length;)
            {
                var count = (int)Math.Min(buffer.Length, length - offset);
                await store.ReadAsync(offset, buffer.AsMemory(0, count), token).ConfigureAwait(false);
                await replacement.WriteAsync(offset, buffer.AsMemory(0, count), token).ConfigureAwait(false); offset += count;
            }
            var previous = store; store = replacement; Spilled = installed = true;
            await previous.DisposeAsync().ConfigureAwait(false);
        }
        finally { CryptographicOperations.ZeroMemory(buffer); if (!installed) await replacement.DisposeAsync().ConfigureAwait(false); }
    }
    public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default)
    {
        await gate.WaitAsync(token).ConfigureAwait(false);
        try { ObjectDisposedException.ThrowIf(disposed, this); await store.ReadAsync(offset, destination, token).ConfigureAwait(false); }
        finally { gate.Release(); }
    }
    public async ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default)
    {
        MemoryBlockStore.CheckRange(offset, source.Length); await gate.WaitAsync(token).ConfigureAwait(false);
        try { ObjectDisposedException.ThrowIf(disposed, this); if (!source.IsEmpty) await Grow(offset + source.Length, token).ConfigureAwait(false); await store.WriteAsync(offset, source, token).ConfigureAwait(false); }
        finally { gate.Release(); }
    }
    public async ValueTask<long> GetLengthAsync(CancellationToken token = default)
    {
        await gate.WaitAsync(token).ConfigureAwait(false);
        try { ObjectDisposedException.ThrowIf(disposed, this); return await store.GetLengthAsync(token).ConfigureAwait(false); }
        finally { gate.Release(); }
    }
    public async ValueTask SetLengthAsync(long length, CancellationToken token = default)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(length); await gate.WaitAsync(token).ConfigureAwait(false);
        try { ObjectDisposedException.ThrowIf(disposed, this); await Grow(length, token).ConfigureAwait(false); await store.SetLengthAsync(length, token).ConfigureAwait(false); }
        finally { gate.Release(); }
    }
    public ValueTask SyncAsync(CancellationToken token = default) { token.ThrowIfCancellationRequested(); ObjectDisposedException.ThrowIf(disposed, this); return ValueTask.CompletedTask; }
    public async ValueTask DisposeAsync()
    {
        await gate.WaitAsync().ConfigureAwait(false);
        try { if (disposed) return; disposed = true; await store.DisposeAsync().ConfigureAwait(false); }
        finally { gate.Release(); }
    }
}
