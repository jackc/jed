using System.Security.Cryptography;

namespace Jed;

public enum ScratchEncryption { Auto, On }

internal interface IScratchDeviceHost
{
    ValueTask<IBlockStore> CreateAsync(string identity, string? directory, CancellationToken token);
}

internal sealed class NativeScratchDeviceHost : IScratchDeviceHost
{
    internal static readonly NativeScratchDeviceHost Instance = new();
    private NativeScratchDeviceHost() { }
    public ValueTask<IBlockStore> CreateAsync(string identity, string? directory, CancellationToken token)
    {
        token.ThrowIfCancellationRequested();
        return ValueTask.FromResult<IBlockStore>(new ScratchFileDevice(Path.Combine(directory ?? Path.GetTempPath(), "jed-scratch-" + identity)));
    }
}

// Scratch capability is host-owned and independent of the database directory.
// Each file owns its key; disposing one cannot affect another active operator.
internal sealed class ScratchHost(bool encrypted, Func<int, byte[]> entropy, string? directory = null, IScratchDeviceHost? devices = null) : IAsyncDisposable
{
    private readonly object entropyGate = new();
    private readonly object lifetimeGate = new();
    private readonly HashSet<OwnedScratchStore> active = [];
    private bool disposed;
    private Task? releasing;
    internal bool Encrypted { get; } = encrypted;
    private byte[] Draw(int count) { lock (entropyGate) return StorageKeyBlock.Entropy(entropy, count); }
    internal async ValueTask<IBlockStore> CreateAsync(CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested();
        lock (lifetimeGate) ObjectDisposedException.ThrowIf(disposed, this);
        byte[]? key = null; byte[]? identity = null; IBlockStore? device = null; IBlockStore? created = null;
        try
        {
            if (Encrypted) key = Draw(32);
            identity = Draw(16);
            device = await (devices ?? NativeScratchDeviceHost.Instance).CreateAsync(Convert.ToHexStringLower(identity), directory, token).ConfigureAwait(false);
            IBlockStore result = created = Encrypted ? new EncryptedScratchStore(device, key!, identity, Draw) : device;
            var owned = new OwnedScratchStore(result, this);
            lock (lifetimeGate)
            {
                ObjectDisposedException.ThrowIf(disposed, this);
                active.Add(owned);
            }
            created = null; device = null; return owned;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.IoError, "cannot create scratch storage: " + error.Message); }
        finally
        {
            try { if (created is not null) await created.DisposeAsync().ConfigureAwait(false); else if (device is not null) await device.DisposeAsync().ConfigureAwait(false); }
            finally { if (key is not null) CryptographicOperations.ZeroMemory(key); if (identity is not null) CryptographicOperations.ZeroMemory(identity); }
        }
    }
    internal SpillableBlockStore CreateSpillable(long residentBytes) => new(this, residentBytes);
    public ValueTask DisposeAsync()
    {
        lock (lifetimeGate)
        {
            disposed = true;
            releasing ??= Release(active.ToArray());
            return new(releasing);
        }
    }
    private static async Task Release(OwnedScratchStore[] stores)
    {
        Exception? failure = null;
        foreach (var store in stores)
            try { await store.DisposeAsync().ConfigureAwait(false); } catch (Exception error) { failure ??= error; }
        if (failure is not null) System.Runtime.ExceptionServices.ExceptionDispatchInfo.Capture(failure).Throw();
    }
    private sealed class OwnedScratchStore(IBlockStore inner, ScratchHost owner) : IBlockStore
    {
        private readonly object gate = new();
        private Task? releasing;
        public ValueTask ReadAsync(long offset, Memory<byte> bytes, CancellationToken token = default) => inner.ReadAsync(offset, bytes, token);
        public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> bytes, CancellationToken token = default) => inner.WriteAsync(offset, bytes, token);
        public ValueTask<long> GetLengthAsync(CancellationToken token = default) => inner.GetLengthAsync(token);
        public ValueTask SetLengthAsync(long length, CancellationToken token = default) => inner.SetLengthAsync(length, token);
        public ValueTask SyncAsync(CancellationToken token = default) => inner.SyncAsync(token);
        public ValueTask DisposeAsync()
        {
            lock (gate) { releasing ??= Release(); return new(releasing); }
        }
        private async Task Release()
        {
            try { await inner.DisposeAsync().ConfigureAwait(false); }
            finally { lock (owner.lifetimeGate) owner.active.Remove(this); }
        }
    }

}

internal sealed class ScratchFileDevice : IBlockStore
{
    private readonly FileStream stream;
    internal ScratchFileDevice(string path)
    {
        var options = new FileStreamOptions
        {
            Mode = FileMode.CreateNew, Access = FileAccess.ReadWrite, Share = FileShare.None,
            BufferSize = 1, Options = FileOptions.Asynchronous | FileOptions.RandomAccess | FileOptions.DeleteOnClose
        };
        if (!OperatingSystem.IsWindows()) options.UnixCreateMode = UnixFileMode.UserRead | UnixFileMode.UserWrite;
        stream = new(path, options);
    }
    public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default)
    {
        MemoryBlockStore.CheckRange(offset, destination.Length);
        try
        {
            while (!destination.IsEmpty)
            {
                var count = await RandomAccess.ReadAsync(stream.SafeFileHandle, destination, offset, token).ConfigureAwait(false);
                if (count == 0) throw new EndOfStreamException("short scratch read");
                offset += count; destination = destination[count..];
            }
        }
        catch (IOException error) { throw new SqlException(SqlStates.IoError, error.Message); }
    }
    public async ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default)
    {
        MemoryBlockStore.CheckRange(offset, source.Length);
        try { await RandomAccess.WriteAsync(stream.SafeFileHandle, source, offset, token).ConfigureAwait(false); }
        catch (IOException error) { throw new SqlException(SqlStates.IoError, error.Message); }
    }
    public ValueTask<long> GetLengthAsync(CancellationToken token = default)
    { token.ThrowIfCancellationRequested(); return new(RandomAccess.GetLength(stream.SafeFileHandle)); }
    public ValueTask SetLengthAsync(long length, CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested(); ArgumentOutOfRangeException.ThrowIfNegative(length);
        try { RandomAccess.SetLength(stream.SafeFileHandle, length); return ValueTask.CompletedTask; }
        catch (IOException error) { throw new SqlException(SqlStates.IoError, error.Message); }
    }
    public ValueTask SyncAsync(CancellationToken token = default) { token.ThrowIfCancellationRequested(); return ValueTask.CompletedTask; }
    public ValueTask DisposeAsync() => stream.DisposeAsync();
}
