namespace Jed;

/// <summary>An owned file credential. Dispose it after open/create completes; database handles retain only their unwrapped data key.</summary>
public sealed class DatabaseCredential : IDisposable
{
    internal StorageCredential Value { get; }
    private DatabaseCredential(StorageCredential value) => Value = value;
    public static DatabaseCredential FromKey(ReadOnlySpan<byte> key) => new(StorageCredential.HostKey(key));
    public static DatabaseCredential FromPassphrase(ReadOnlySpan<byte> utf8, uint memoryKiB = 65536, uint passes = 3, uint lanes = 4) =>
        new(StorageCredential.Passphrase(utf8, memoryKiB, passes, lanes));
    public void Dispose() => Value.Dispose();
}
