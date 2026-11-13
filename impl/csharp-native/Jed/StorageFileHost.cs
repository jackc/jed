namespace Jed;

// Filesystem policy belongs to the host, alongside positioned byte devices.
// Keeping directory operations here lets deterministic crash hosts observe
// creation, replacement and directory durability through the actual engine.
// This is an internal adapter seam; public file access still acquires protocol-v2
// locks before the host opens the database device.
internal interface IFileHost
{
    ValueTask<bool> ExistsAsync(string path, CancellationToken token = default);
    ValueTask<IBlockStore> OpenAsync(string path, FileMode mode, bool readOnly = false, CancellationToken token = default);
    // Replace a reserved maintenance sibling. Native hosts unlink first and
    // then claim it exclusively; crash hosts model this whole creation step.
    async ValueTask<IBlockStore> CreateTemporaryAsync(string path, CancellationToken token = default)
    {
        await DeleteAsync(path, token).ConfigureAwait(false);
        return await OpenAsync(path, FileMode.CreateNew, token: token).ConfigureAwait(false);
    }
    ValueTask DeleteAsync(string path, CancellationToken token = default);
    ValueTask MoveAsync(string source, string destination, bool overwrite, CancellationToken token = default);
    ValueTask<int?> GetPermissionsAsync(string path, CancellationToken token = default);
    ValueTask SetPermissionsAsync(string path, int? permissions, CancellationToken token = default);
    ValueTask SyncDirectoryAsync(string path, CancellationToken token = default);
}

internal sealed class NativeFileHost : IFileHost
{
    internal static readonly NativeFileHost Instance = new();
    private NativeFileHost() { }

    public ValueTask<bool> ExistsAsync(string path, CancellationToken token = default)
    { token.ThrowIfCancellationRequested(); return ValueTask.FromResult(File.Exists(path) || Directory.Exists(path)); }

    public ValueTask<IBlockStore> OpenAsync(string path, FileMode mode, bool readOnly = false, CancellationToken token = default)
    { token.ThrowIfCancellationRequested(); return ValueTask.FromResult<IBlockStore>(new FileBlockStore(path, mode, readOnly)); }

    public ValueTask DeleteAsync(string path, CancellationToken token = default)
    { token.ThrowIfCancellationRequested(); File.Delete(path); return ValueTask.CompletedTask; }

    public ValueTask MoveAsync(string source, string destination, bool overwrite, CancellationToken token = default)
    { token.ThrowIfCancellationRequested(); File.Move(source, destination, overwrite); return ValueTask.CompletedTask; }

    public ValueTask<int?> GetPermissionsAsync(string path, CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested();
        return ValueTask.FromResult<int?>(OperatingSystem.IsLinux() ? (int)File.GetUnixFileMode(path) : null);
    }

    public ValueTask SetPermissionsAsync(string path, int? permissions, CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested();
        if (permissions is { } mode && OperatingSystem.IsLinux()) File.SetUnixFileMode(path, (UnixFileMode)mode);
        return ValueTask.CompletedTask;
    }

    public ValueTask SyncDirectoryAsync(string path, CancellationToken token = default)
    {
        token.ThrowIfCancellationRequested();
        FileBlockStore.SyncDirectory(Path.GetDirectoryName(Path.GetFullPath(path))!);
        return ValueTask.CompletedTask;
    }
}
