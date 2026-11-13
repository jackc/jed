using System.Collections.Concurrent;
using System.Diagnostics;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace Jed;

// Linux host seam. Engine algorithms never call libc; coordination deliberately
// uses flock, not FileStream.Lock's incompatible fcntl byte-range primitive.
internal sealed class FileClaim : IDisposable
{
    private static readonly ConcurrentDictionary<string, byte> OpenPaths = new(StringComparer.Ordinal);
    internal string Path { get; }
    internal int ProcessId { get; } = Environment.ProcessId;
    private bool disposed;
    private FileClaim(string path) { Path = path; }
    internal static FileClaim Acquire(string path, bool creating)
    {
        LinuxFileHost.RequireLinux();
        var full = System.IO.Path.GetFullPath(path);
        var normalized = creating
            ? System.IO.Path.Combine(LinuxFileHost.RealPath(System.IO.Path.GetDirectoryName(full)!), System.IO.Path.GetFileName(full))
            : LinuxFileHost.RealPath(full);
        LinuxFileHost.RequireLocalFilesystem(creating ? System.IO.Path.GetDirectoryName(normalized)! : normalized);
        var info = LinuxFileHost.TryStat(normalized, false);
        if (info is { Links: > 1 }) throw new SqlException(SqlStates.ObjectInUse, "coordinated file opens reject hard links: " + normalized);
        if (!OpenPaths.TryAdd(normalized, 0)) throw new SqlException(SqlStates.ObjectInUse, "database already open in this process: " + normalized);
        return new(normalized);
    }
    internal void VerifyProcess()
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        if (Environment.ProcessId != ProcessId) throw new SqlException(SqlStates.ObjectInUse, "database handle inherited across fork; reopen it in the child");
    }
    public void Dispose()
    {
        if (disposed) return;
        disposed = true; OpenPaths.TryRemove(Path, out _);
    }
}

internal sealed class FileLockBundle : IDisposable
{
    internal FileClaim Claim { get; }
    internal string Path { get; }
    internal bool OwnsPresence { get; private set; }
    private readonly SafeFileHandle directory;
    private readonly SafeFileHandle presence;
    private readonly SafeFileHandle arrival;
    private bool hasArrival;
    private bool disposed;
    private FileLockBundle(FileClaim claim, string path, SafeFileHandle dir, SafeFileHandle presence, SafeFileHandle arrival)
    { Claim = claim; Path = path; directory = dir; this.presence = presence; this.arrival = arrival; }

    // The claim belongs to the Database across owner/client role changes. Closing
    // the bundle never releases it; the Database disposes its claim on final close.
    internal static async ValueTask<FileLockBundle> PrepareAsync(FileClaim claim, long timeoutMilliseconds = 5000, CancellationToken cancellationToken = default)
    {
        claim.VerifyProcess();
        if (timeoutMilliseconds < 0) throw new ArgumentOutOfRangeException(nameof(timeoutMilliseconds));
        var path = claim.Path + ".lock";
        SafeFileHandle? dir = null, presence = null, arrival = null;
        FileLockBundle? bundle = null;
        try
        {
            Directory.CreateDirectory(path);
            dir = LinuxFileHost.OpenDirectory(path);
            using (LinuxFileHost.OpenEntry(dir, "protocol-v2", writable: false)) { }
            presence = LinuxFileHost.OpenEntry(dir, "presence", writable: true);
            arrival = LinuxFileHost.OpenEntry(dir, "arrival", writable: true);
            bundle = new(claim, path, dir, presence, arrival);
            var timer = Stopwatch.StartNew();
            while (!LinuxFileHost.TryLock(arrival, exclusive: false))
            {
                if (timer.ElapsedMilliseconds >= timeoutMilliseconds) throw bundle.Timeout();
                await Task.Delay((int)Math.Min(10, Math.Max(1, timeoutMilliseconds - timer.ElapsedMilliseconds)), cancellationToken).ConfigureAwait(false);
            }
            bundle.hasArrival = true;
            bundle.ValidateMarker();
            return bundle;
        }
        catch (Exception error)
        {
            if (bundle is not null) bundle.Dispose();
            else { arrival?.Dispose(); presence?.Dispose(); dir?.Dispose(); }
            if (error is IOException or UnauthorizedAccessException) throw new SqlException(SqlStates.IoError, "cannot prepare persistent database lock bundle: " + error.Message);
            throw;
        }
    }
    private void ValidateMarker()
    {
        foreach (var entry in Directory.EnumerateFileSystemEntries(Path, "protocol-v*"))
        {
            var name = System.IO.Path.GetFileName(entry);
            if (name is not ("protocol-v1" or "protocol-v2")) throw new SqlException(SqlStates.FeatureNotSupported, "unsupported database lock protocol: " + name);
        }
        using var marker = LinuxFileHost.OpenEntry(directory, "protocol-v2", writable: false);
    }
    internal bool TryBecomeOwner()
    {
        Check();
        if (OwnsPresence) return true;
        if (!LinuxFileHost.TryLock(presence, exclusive: true)) return false;
        OwnsPresence = true;
        ReleaseArrival();
        return true;
    }
    internal async ValueTask AcquireExclusiveAsync(long timeoutMilliseconds = 5000, CancellationToken cancellationToken = default)
    {
        if (timeoutMilliseconds < 0) throw new ArgumentOutOfRangeException(nameof(timeoutMilliseconds));
        var timer = Stopwatch.StartNew();
        while (!TryBecomeOwner())
        {
            if (timer.ElapsedMilliseconds >= timeoutMilliseconds) throw Timeout();
            await Task.Delay((int)Math.Min(10, Math.Max(1, timeoutMilliseconds - timer.ElapsedMilliseconds)), cancellationToken).ConfigureAwait(false);
        }
    }
    internal void ReleaseArrival()
    {
        Check();
        if (!hasArrival) return;
        LinuxFileHost.Unlock(arrival); hasArrival = false;
    }
    internal PageOwnerRecord? ReadOwner()
    {
        Check();
        try
        {
            using var handle = LinuxFileHost.OpenExistingEntry(directory, "owner");
            var size = RandomAccess.GetLength(handle);
            if (size is < 43 or > 143) return null;
            var bytes = new byte[(int)size];
            var offset = 0;
            while (offset < bytes.Length)
            {
                var count = RandomAccess.Read(handle, bytes.AsSpan(offset), offset);
                if (count == 0) return null;
                offset += count;
            }
            return PageWire.DecodeOwner(bytes);
        }
        catch (SqlException) { return null; }
        catch (IOException) { return null; }
        catch (UnauthorizedAccessException) { return null; }
    }
    internal async ValueTask PublishOwnerAsync(PageOwnerRecord owner, CancellationToken cancellationToken = default)
    {
        CheckOwner();
        if (owner.Uid != LinuxFileHost.EffectiveUid || owner.Pid != (uint)Claim.ProcessId) throw new ArgumentException("owner identity differs from this process");
        var bytes = PageWire.EncodeOwner(owner);
        var temporary = System.IO.Path.Combine(Path, "owner.tmp-" + Convert.ToHexStringLower(owner.Incarnation));
        try
        {
            await using (var file = new FileStream(temporary, FileMode.CreateNew, FileAccess.Write, FileShare.None, 4096, FileOptions.Asynchronous))
                await file.WriteAsync(bytes, cancellationToken).ConfigureAwait(false);
            File.Move(temporary, System.IO.Path.Combine(Path, "owner"), overwrite: true);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        { throw new SqlException(SqlStates.IoError, "cannot publish page-service owner: " + error.Message); }
        finally { try { File.Delete(temporary); } catch (IOException) { } catch (UnauthorizedAccessException) { } }
    }
    internal string? SelectEndpoint(ReadOnlySpan<byte> incarnation)
    {
        CheckOwner();
        if (incarnation.Length != 16) throw new ArgumentException("incarnation must be 16 bytes");
        var userDirectory = System.IO.Path.Combine(Path, "uid-" + LinuxFileHost.EffectiveUid.ToString(System.Globalization.CultureInfo.InvariantCulture));
        var endpoint = System.IO.Path.Combine(userDirectory, "endpoint");
        if (PageWire.Utf8.GetByteCount(endpoint) <= 100 && LinuxFileHost.PrivateDirectory(userDirectory, create: true)) return endpoint;
        var suffix = "jed-" + Convert.ToHexStringLower(incarnation[..8]) + ".sock";
        var fallback = "/tmp/jed-" + LinuxFileHost.EffectiveUid.ToString(System.Globalization.CultureInfo.InvariantCulture);
        foreach (var candidate in new[] { Environment.GetEnvironmentVariable("XDG_RUNTIME_DIR"), fallback })
        {
            if (string.IsNullOrEmpty(candidate) || !System.IO.Path.IsPathFullyQualified(candidate)) continue;
            endpoint = System.IO.Path.Combine(candidate, suffix);
            if (LinuxFileHost.PrivateDirectory(candidate, create: candidate == fallback) && PageWire.Utf8.GetByteCount(endpoint) <= 100) return endpoint;
        }
        return null;
    }
    internal void RemoveEndpoint(string endpoint)
    {
        CheckOwner();
        // Caller selects this path with SelectEndpoint and closes its listener first.
        try { File.Delete(endpoint); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { throw new SqlException(SqlStates.IoError, "cannot remove page endpoint: " + error.Message); }
    }
    private SqlException Timeout()
    {
        var owner = ReadOwner();
        var detail = owner is null ? "" : owner.Uid != LinuxFileHost.EffectiveUid ? "; owned by another user's process" : !owner.Serving ? "; owned by a non-serving process" : "";
        return new(SqlStates.ObjectInUse, "database lock timeout: " + Claim.Path + detail);
    }
    private void Check() { ObjectDisposedException.ThrowIf(disposed, this); Claim.VerifyProcess(); }
    private void CheckOwner() { Check(); if (!OwnsPresence) throw new InvalidOperationException("presence lock is not held"); }
    public void Dispose()
    {
        if (disposed) return;
        disposed = true;
        // A child must never unlock the inherited open-file description or delete
        // its parent's record. Closing its own descriptor copy is sufficient.
        if (Environment.ProcessId == Claim.ProcessId)
        {
            if (OwnsPresence)
            {
                try { File.Delete(System.IO.Path.Combine(Path, "owner")); } catch (IOException) { } catch (UnauthorizedAccessException) { }
            }
        }
        arrival.Dispose(); presence.Dispose(); directory.Dispose();
        OwnsPresence = false; hasArrival = false;
    }
}

internal static class LinuxFileHost
{
    private const int CloseOnExec = 0x80000, NoFollow = 0x20000, DirectoryOnly = 0x10000;
    private const int Shared = 1, Exclusive = 2, Nonblocking = 4, UnlockMode = 8;
    internal static uint EffectiveUid => GetEffectiveUid();
    internal static void RequireLinux()
    { if (!OperatingSystem.IsLinux() || RuntimeInformation.ProcessArchitecture != Architecture.X64) throw new SqlException(SqlStates.FeatureNotSupported, "C# file coordination currently supports Linux x86-64 only"); }
    internal static void RequireLocalFilesystem(string path)
    {
        if (StatFileSystem(path, out var info) != 0) throw Io("stat filesystem", Marshal.GetLastPInvokeError());
        if (info.Type is 0x6969 or 0xff534d42 or 0xfe534d42)
            throw new SqlException(SqlStates.FeatureNotSupported, "coordinated database access requires a supported local filesystem");
    }
    internal static string RealPath(string path)
    {
        var pointer = ResolveRealPath(path, nint.Zero);
        if (pointer == nint.Zero)
        {
            var errno = Marshal.GetLastPInvokeError();
            throw new SqlException(errno == 2 ? SqlStates.UndefinedFile : SqlStates.IoError, "cannot normalize database path: " + path + " (errno " + errno + ")");
        }
        try { return Marshal.PtrToStringUTF8(pointer) ?? throw new IOException("empty realpath result"); }
        finally { Free(pointer); }
    }
    internal static SafeFileHandle OpenDirectory(string path) => Handle(Open(path, CloseOnExec | NoFollow | DirectoryOnly, 0), "open lock directory");
    internal static SafeFileHandle OpenEntry(SafeFileHandle directory, string name, bool writable)
    {
        var handle = Handle(OpenAt(directory, name, CloseOnExec | NoFollow | 0x40 | (writable ? 2 : 0), 0x1b6), "open lock entry");
        try { CheckRegular(handle); return handle; } catch { handle.Dispose(); throw; }
    }
    internal static SafeFileHandle OpenExistingEntry(SafeFileHandle directory, string name)
    {
        var handle = Handle(OpenAt(directory, name, CloseOnExec | NoFollow, 0), "open owner record");
        try { CheckRegular(handle); return handle; } catch { handle.Dispose(); throw; }
    }
    private static SafeFileHandle Handle(int descriptor, string operation)
    {
        if (descriptor < 0) throw Io(operation, Marshal.GetLastPInvokeError());
        return new SafeFileHandle((nint)descriptor, ownsHandle: true);
    }
    private static void CheckRegular(SafeFileHandle handle)
    {
        if (StatHandle(handle, "", 0x1100, 0x7ff, out var info) != 0) throw Io("stat lock entry", Marshal.GetLastPInvokeError());
        if ((info.Mode & 0xf000) != 0x8000) throw new SqlException(SqlStates.IoError, "lock bundle entry is not a regular file");
    }
    internal static bool TryLock(SafeFileHandle handle, bool exclusive)
    {
        while (Flock(handle, (exclusive ? Exclusive : Shared) | Nonblocking) != 0)
        {
            var errno = Marshal.GetLastPInvokeError();
            if (errno == 4) continue;
            if (errno == 11) return false;
            if (errno is 38 or 95) throw new SqlException(SqlStates.FeatureNotSupported, "filesystem does not support crash-clean flock locks");
            throw Io("flock", errno);
        }
        return true;
    }
    internal static void Unlock(SafeFileHandle handle)
    { while (Flock(handle, UnlockMode) != 0) { var errno = Marshal.GetLastPInvokeError(); if (errno != 4) throw Io("flock unlock", errno); } }
    internal static StatInfo? TryStat(string path, bool noFollow)
    {
        if (StatPath(-100, path, noFollow ? 0x100 : 0, 0x7ff, out var info) == 0) return info;
        var errno = Marshal.GetLastPInvokeError();
        if (errno == 2) return null;
        throw Io("stat file", errno);
    }
    internal static bool PrivateDirectory(string path, bool create)
    {
        try
        {
            if (create && Mkdir(path, 0x1c0) != 0 && Marshal.GetLastPInvokeError() != 17) return false;
            var info = TryStat(path, noFollow: true);
            return info is { } found && found.Uid == EffectiveUid && found.Mode == (0x4000 | 0x1c0);
        }
        catch (SqlException) { return false; }
    }
    private static SqlException Io(string operation, int errno) => new(SqlStates.IoError, operation + " failed (errno " + errno + ")");
    [StructLayout(LayoutKind.Explicit, Size = 120)]
    private struct FileSystemInfo { [FieldOffset(0)] internal long Type; }
    [DllImport("libc", EntryPoint = "statfs", SetLastError = true)]
    private static extern int StatFileSystem([MarshalAs(UnmanagedType.LPUTF8Str)] string path, out FileSystemInfo info);
    [StructLayout(LayoutKind.Explicit, Size = 256)]
    internal struct StatInfo
    {
        [FieldOffset(16)] internal uint Links;
        [FieldOffset(20)] internal uint Uid;
        [FieldOffset(28)] internal ushort Mode;
    }
    [DllImport("libc", EntryPoint = "open", SetLastError = true)] private static extern int Open([MarshalAs(UnmanagedType.LPUTF8Str)] string path, int flags, uint mode);
    [DllImport("libc", EntryPoint = "openat", SetLastError = true)] private static extern int OpenAt(SafeFileHandle directory, [MarshalAs(UnmanagedType.LPUTF8Str)] string path, int flags, uint mode);
    [DllImport("libc", EntryPoint = "flock", SetLastError = true)] private static extern int Flock(SafeFileHandle handle, int operation);
    [DllImport("libc", EntryPoint = "realpath", SetLastError = true)] private static extern nint ResolveRealPath([MarshalAs(UnmanagedType.LPUTF8Str)] string path, nint resolved);
    [DllImport("libc", EntryPoint = "free")] private static extern void Free(nint memory);
    [DllImport("libc", EntryPoint = "geteuid")] private static extern uint GetEffectiveUid();
    [DllImport("libc", EntryPoint = "mkdir", SetLastError = true)] private static extern int Mkdir([MarshalAs(UnmanagedType.LPUTF8Str)] string path, uint mode);
    [DllImport("libc", EntryPoint = "statx", SetLastError = true)] private static extern int StatPath(int directory, [MarshalAs(UnmanagedType.LPUTF8Str)] string path, int flags, uint mask, out StatInfo info);
    [DllImport("libc", EntryPoint = "statx", SetLastError = true)] private static extern int StatHandle(SafeFileHandle directory, [MarshalAs(UnmanagedType.LPUTF8Str)] string path, int flags, uint mask, out StatInfo info);
}
