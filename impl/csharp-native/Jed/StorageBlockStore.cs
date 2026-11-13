using Microsoft.Win32.SafeHandles;
using System.Runtime.InteropServices;

namespace Jed;

// The byte host owns no page policy, SQLSTATE mapping, locks, or transaction state.
// Reads and writes are exact and positioned. An I/O failure may leave a partial write.
internal interface IBlockStore : IAsyncDisposable
{
    ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default);
    ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default);
    ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default);
    ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default);
    ValueTask SyncAsync(CancellationToken cancellationToken = default);
}

internal sealed class MemoryBlockStore : IBlockStore
{
    // Segments keep the host usable beyond the CLR's single-array size limit.
    private const int SegmentSize = 65536;
    private readonly Dictionary<long, byte[]> segments = [];
    private readonly object gate = new();
    private long length;
    private bool disposed;

    internal MemoryBlockStore(ReadOnlySpan<byte> initial = default)
    {
        length = initial.Length;
        CopyIn(0, initial);
    }

    public ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        lock (gate)
        {
            Check(); CheckRange(offset, destination.Length);
            if (offset + destination.Length > length) throw new EndOfStreamException("short block-store read");
            var remaining = destination.Span;
            while (!remaining.IsEmpty)
            {
                var segmentOffset = (int)(offset % SegmentSize);
                var count = Math.Min(remaining.Length, SegmentSize - segmentOffset);
                if (segments.TryGetValue(offset / SegmentSize, out var segment))
                    segment.AsSpan(segmentOffset, count).CopyTo(remaining);
                else remaining[..count].Clear();
                remaining = remaining[count..]; offset += count;
            }
        }
        return ValueTask.CompletedTask;
    }

    public ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        lock (gate)
        {
            Check(); CheckRange(offset, source.Length);
            CopyIn(offset, source.Span);
            if (!source.IsEmpty) length = Math.Max(length, offset + source.Length);
        }
        return ValueTask.CompletedTask;
    }

    public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        lock (gate) { Check(); return ValueTask.FromResult(length); }
    }

    public ValueTask SetLengthAsync(long value, CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        ArgumentOutOfRangeException.ThrowIfNegative(value);
        lock (gate)
        {
            Check();
            if (value < length)
            {
                var firstRemoved = value / SegmentSize + (value % SegmentSize == 0 ? 0 : 1);
                foreach (var key in segments.Keys.Where(key => key >= firstRemoved).ToArray()) segments.Remove(key);
                if (value % SegmentSize != 0 && segments.TryGetValue(value / SegmentSize, out var tail))
                    tail.AsSpan((int)(value % SegmentSize)).Clear();
            }
            length = value;
        }
        return ValueTask.CompletedTask;
    }

    public ValueTask SyncAsync(CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        lock (gate) Check();
        return ValueTask.CompletedTask;
    }

    public ValueTask DisposeAsync()
    {
        lock (gate) { disposed = true; segments.Clear(); length = 0; }
        return ValueTask.CompletedTask;
    }

    private void Check() => ObjectDisposedException.ThrowIf(disposed, this);

    internal static void CheckRange(long offset, int count)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(offset);
        if (offset > long.MaxValue - count) throw new ArgumentOutOfRangeException(nameof(offset));
    }

    private void CopyIn(long offset, ReadOnlySpan<byte> source)
    {
        while (!source.IsEmpty)
        {
            var segmentIndex = offset / SegmentSize;
            var segmentOffset = (int)(offset % SegmentSize);
            var count = Math.Min(source.Length, SegmentSize - segmentOffset);
            if (!segments.TryGetValue(segmentIndex, out var segment)) segments.Add(segmentIndex, segment = new byte[SegmentSize]);
            source[..count].CopyTo(segment.AsSpan(segmentOffset));
            source = source[count..]; offset += count;
        }
    }
}

// The host layer must acquire the protocol-v2 lock bundle BEFORE opening this
// device. FileShare is deliberately not used as a substitute for that protocol.
internal sealed class FileBlockStore : IBlockStore
{
    private readonly FileStream stream;
    private readonly string directory;
    private SafeFileHandle Handle => stream.SafeFileHandle;

    internal FileBlockStore(string path, FileMode mode, bool readOnly = false)
    {
        directory = Path.GetDirectoryName(Path.GetFullPath(path))!;
        stream = new FileStream(path, mode, readOnly ? FileAccess.Read : FileAccess.ReadWrite,
            FileShare.ReadWrite | FileShare.Delete, 1, FileOptions.Asynchronous | FileOptions.RandomAccess);
    }

    public async ValueTask ReadAsync(long offset, Memory<byte> destination, CancellationToken cancellationToken = default)
    {
        MemoryBlockStore.CheckRange(offset, destination.Length);
        cancellationToken.ThrowIfCancellationRequested();
        while (!destination.IsEmpty)
        {
            var count = await RandomAccess.ReadAsync(Handle, destination, offset, cancellationToken).ConfigureAwait(false);
            if (count == 0) throw new EndOfStreamException("short block-store read");
            destination = destination[count..]; offset += count;
        }
    }

    public async ValueTask WriteAsync(long offset, ReadOnlyMemory<byte> source, CancellationToken cancellationToken = default)
    {
        MemoryBlockStore.CheckRange(offset, source.Length);
        cancellationToken.ThrowIfCancellationRequested();
        // RandomAccess.WriteAsync completes the entire buffer or throws.
        await RandomAccess.WriteAsync(Handle, source, offset, cancellationToken).ConfigureAwait(false);
    }

    public ValueTask<long> GetLengthAsync(CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        return ValueTask.FromResult(RandomAccess.GetLength(Handle));
    }

    public async ValueTask SetLengthAsync(long length, CancellationToken cancellationToken = default)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(length);
        cancellationToken.ThrowIfCancellationRequested();
        var before = RandomAccess.GetLength(Handle);
        if (length <= before) { RandomAccess.SetLength(Handle, length); return; }
        // Actual zero writes, rather than sparse truncate, allocate the region
        // before the full metadata barrier. Only a fixed-size scratch is needed.
        var zeros = new byte[65536];
        for (var offset = before; offset < length;)
        {
            var count = (int)Math.Min(zeros.Length, length - offset);
            await RandomAccess.WriteAsync(Handle, zeros.AsMemory(0, count), offset, cancellationToken).ConfigureAwait(false);
            offset += count;
        }
        stream.Flush(flushToDisk: true);
    }

    public ValueTask SyncAsync(CancellationToken cancellationToken = default)
    {
        cancellationToken.ThrowIfCancellationRequested();
        // .NET exposes no asynchronous durable fsync API. A full fsync is the
        // sanctioned, correct fallback for a host without a data-only barrier.
        // FlushAsync alone is NOT a durable barrier; do not hide fsync in Task.Run.
        stream.Flush(flushToDisk: true);
        return ValueTask.CompletedTask;
    }

    public ValueTask DisposeAsync() => stream.DisposeAsync();

    internal void SyncDirectory() => SyncDirectory(directory);

    internal static void SyncDirectory(string directory)
    {
        // Linux host seam, not an engine dependency. Directory sync is best
        // effort per validated-cow.md; data-file sync errors still propagate.
        if (!OperatingSystem.IsLinux()) throw new PlatformNotSupportedException("C# file durability currently targets Linux");
        const int directoryOnly = 0x10000, closeOnExec = 0x80000;
        var fd = OpenDirectory(directory, directoryOnly | closeOnExec);
        if (fd < 0) return;
        using var handle = new SafeFileHandle((nint)fd, ownsHandle: true);
        while (SyncDirectoryHandle(handle) != 0 && Marshal.GetLastPInvokeError() == 4) { }
    }

    [DllImport("libc", EntryPoint = "open", SetLastError = true)]
    private static extern int OpenDirectory([MarshalAs(UnmanagedType.LPUTF8Str)] string path, int flags);

    [DllImport("libc", EntryPoint = "fsync", SetLastError = true)]
    private static extern int SyncDirectoryHandle(SafeFileHandle handle);
}
