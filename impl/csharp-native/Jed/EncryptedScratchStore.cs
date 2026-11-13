using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Jed;

// Ephemeral encrypted.md §11 framing. Counters survive truncation; only live
// blocks authenticate. Keys and counters never enter the file.
internal sealed class EncryptedScratchStore : IBlockStore
{
    internal const int BlockSize = 4096, StoredSize = 4136;
    private readonly IBlockStore device;
    private readonly Xaes256Gcm cipher;
    private readonly byte[] identity;
    private readonly Func<int, byte[]> entropy;
    private readonly Dictionary<long, ulong> counters = [];
    private readonly HashSet<long> live = [];
    private readonly SemaphoreSlim gate = new(1, 1);
    private long length;
    private bool disposed, failed;
    internal EncryptedScratchStore(IBlockStore device, ReadOnlySpan<byte> key, ReadOnlySpan<byte> identity, Func<int, byte[]> entropy)
    {
        if (identity.Length != 16) throw new ArgumentException("scratch identity requires 16 bytes");
        this.device = device; this.identity = identity.ToArray(); this.entropy = entropy; cipher = new(key);
    }
    private void Check()
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (failed) throw new SqlException(SqlStates.IoError, "scratch storage failed");
    }
    private byte[] Aad(long block, ulong counter)
    {
        var bytes = new byte[38]; "JEDB"u8.CopyTo(bytes); bytes[4] = 1; bytes[5] = 3;
        identity.CopyTo(bytes, 6); BinaryPrimitives.WriteUInt64BigEndian(bytes.AsSpan(22), checked((ulong)block));
        BinaryPrimitives.WriteUInt64BigEndian(bytes.AsSpan(30), counter); return bytes;
    }
    private static long Offset(long block) => block <= long.MaxValue / StoredSize
        ? block * StoredSize : throw new SqlException(SqlStates.IoError, "scratch file exceeds addressable length");
    private async ValueTask<byte[]> Load(long block, CancellationToken token)
    {
        if (!live.Contains(block)) return new byte[BlockSize];
        var stored = new byte[StoredSize];
        await device.ReadAsync(Offset(block), stored, token).ConfigureAwait(false);
        var sealedBytes = new byte[BlockSize + 16]; stored.AsSpan(0, BlockSize).CopyTo(sealedBytes);
        stored.AsSpan(BlockSize + 24, 16).CopyTo(sealedBytes.AsSpan(BlockSize));
        return cipher.Open(stored.AsSpan(BlockSize, 24), sealedBytes, Aad(block, counters[block]))
            ?? throw new SqlException(SqlStates.IoError, "scratch block authentication failed");
    }
    private async ValueTask Save(long block, byte[] plaintext, CancellationToken token)
    {
        var previous = counters.GetValueOrDefault(block);
        if (previous == ulong.MaxValue) throw new SqlException(SqlStates.IoError, "scratch block write counter exhausted");
        var counter = previous + 1; counters[block] = counter;
        var nonce = StorageKeyBlock.Entropy(entropy, 24);
        try
        {
            var sealedBytes = cipher.Seal(nonce, plaintext, Aad(block, counter));
            var stored = new byte[StoredSize]; sealedBytes.AsSpan(0, BlockSize).CopyTo(stored);
            nonce.CopyTo(stored, BlockSize); sealedBytes.AsSpan(BlockSize).CopyTo(stored.AsSpan(BlockSize + 24));
            await device.WriteAsync(Offset(block), stored, token).ConfigureAwait(false); live.Add(block);
        }
        finally { CryptographicOperations.ZeroMemory(nonce); }
    }
    public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken token = default)
    {
        MemoryBlockStore.CheckRange(offset, destination.Length); await gate.WaitAsync(token).ConfigureAwait(false);
        try
        {
            Check(); if (offset + destination.Length > length) throw new SqlException(SqlStates.IoError, "short scratch read");
            while (!destination.IsEmpty)
            {
                var block = offset / BlockSize; var within = (int)(offset % BlockSize);
                var count = Math.Min(destination.Length, BlockSize - within); var plaintext = await Load(block, token).ConfigureAwait(false);
                try { plaintext.AsMemory(within, count).CopyTo(destination); }
                finally { CryptographicOperations.ZeroMemory(plaintext); }
                offset += count; destination = destination[count..];
            }
        }
        catch (IOException error) { failed = true; throw new SqlException(SqlStates.IoError, error.Message); }
        catch (SqlException) { failed = true; throw; }
        finally { gate.Release(); }
    }
    public async ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken token = default)
    {
        MemoryBlockStore.CheckRange(offset, source.Length); await gate.WaitAsync(token).ConfigureAwait(false);
        try
        {
            Check(); if (source.IsEmpty) return; var end = offset + source.Length;
            while (!source.IsEmpty)
            {
                var block = offset / BlockSize; var within = (int)(offset % BlockSize);
                var count = Math.Min(source.Length, BlockSize - within);
                var plaintext = within == 0 && count == BlockSize ? new byte[BlockSize] : await Load(block, token).ConfigureAwait(false);
                try { source[..count].CopyTo(plaintext.AsMemory(within)); await Save(block, plaintext, token).ConfigureAwait(false); }
                finally { CryptographicOperations.ZeroMemory(plaintext); }
                offset += count; source = source[count..];
            }
            length = Math.Max(length, end);
        }
        catch (IOException error) { failed = true; throw new SqlException(SqlStates.IoError, error.Message); }
        catch { failed = true; throw; }
        finally { gate.Release(); }
    }
    public async ValueTask<long> GetLengthAsync(CancellationToken token = default)
    {
        await gate.WaitAsync(token).ConfigureAwait(false);
        try { Check(); return length; } finally { gate.Release(); }
    }
    public async ValueTask SetLengthAsync(long value, CancellationToken token = default)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(value); await gate.WaitAsync(token).ConfigureAwait(false);
        try
        {
            Check(); _ = Offset(value / BlockSize + (value % BlockSize == 0 ? 0 : 1));
            if (value < length)
            {
                var kept = value / BlockSize; var within = (int)(value % BlockSize);
                if (within != 0 && live.Contains(kept))
                {
                    var plaintext = await Load(kept, token).ConfigureAwait(false);
                    try { plaintext.AsSpan(within).Clear(); await Save(kept, plaintext, token).ConfigureAwait(false); }
                    finally { CryptographicOperations.ZeroMemory(plaintext); }
                }
                live.RemoveWhere(block => block >= kept + (within == 0 ? 0 : 1));
                await device.SetLengthAsync(Offset(kept + (within == 0 ? 0 : 1)), token).ConfigureAwait(false);
            }
            length = value;
        }
        catch (IOException error) { failed = true; throw new SqlException(SqlStates.IoError, error.Message); }
        catch { failed = true; throw; }
        finally { gate.Release(); }
    }
    public ValueTask SyncAsync(CancellationToken token = default) { token.ThrowIfCancellationRequested(); Check(); return ValueTask.CompletedTask; }
    public async ValueTask DisposeAsync()
    {
        await gate.WaitAsync().ConfigureAwait(false);
        try
        {
            if (disposed) return; disposed = true;
            try { await device.DisposeAsync().ConfigureAwait(false); }
            finally { cipher.Dispose(); CryptographicOperations.ZeroMemory(identity); counters.Clear(); live.Clear(); }
        }
        finally { gate.Release(); }
    }
}
