using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Jed;

internal sealed class StorageCredential : IDisposable
{
    private byte[] secret;
    private bool disposed;
    internal byte Kind { get; }
    internal uint MemoryKiB { get; }
    internal uint Passes { get; }
    internal uint Lanes { get; }

    private StorageCredential(byte kind, ReadOnlySpan<byte> bytes, uint memoryKiB, uint passes, uint lanes)
    {
        Kind = kind; MemoryKiB = memoryKiB; Passes = passes; Lanes = lanes;
        secret = bytes.ToArray();
    }
    internal static StorageCredential HostKey(ReadOnlySpan<byte> bytes)
    {
        CryptoProvider.CheckKey(bytes); return new(2, bytes, 0, 0, 0);
    }
    internal static StorageCredential Passphrase(ReadOnlySpan<byte> utf8, uint memoryKiB = 65536, uint passes = 3, uint lanes = 4)
    {
        if (!StorageKeyBlock.ValidArgonParameters(memoryKiB, passes, lanes))
            throw new SqlException(SqlStates.InvalidParameterValue, "invalid Argon2id credential parameters");
        // Bytes are already the host's explicit UTF-8 encoding. They never enter
        // SQL or error prose and need no immutable string copy in the engine.
        return new(1, utf8, memoryKiB, passes, lanes);
    }
    internal PageWireCredential WireCopy()
    { ObjectDisposedException.ThrowIf(disposed, this); return new(Kind, secret.ToArray()); }
    internal byte[] Derive(ReadOnlySpan<byte> salt, uint memoryKiB, uint passes, uint lanes)
    {
        ObjectDisposedException.ThrowIf(disposed, this);
        return Kind == 2 ? CryptoProvider.Hkdf(secret, salt, "jed/v1/kek"u8) :
            CryptoProvider.Argon2id(secret, salt, memoryKiB, passes, lanes);
    }
    public void Dispose()
    {
        if (disposed) return;
        disposed = true; CryptographicOperations.ZeroMemory(secret); secret = [];
    }
}

internal sealed record StorageKeySelection(StorageKeyBlock Block, uint Page, bool NeedsRepair);

internal sealed class StorageKeyBlock
{
    private readonly byte[] bytes;
    internal ulong Generation => BinaryPrimitives.ReadUInt64BigEndian(bytes.AsSpan(8));
    internal byte Kind => bytes[7];
    internal uint MemoryKiB => BinaryPrimitives.ReadUInt32BigEndian(bytes.AsSpan(16));
    internal uint Passes => BinaryPrimitives.ReadUInt32BigEndian(bytes.AsSpan(20));
    internal uint Lanes => BinaryPrimitives.ReadUInt32BigEndian(bytes.AsSpan(24));
    internal ReadOnlyMemory<byte> Bytes => bytes;
    private StorageKeyBlock(byte[] bytes) { this.bytes = bytes; }

    internal static bool ValidArgonParameters(uint memoryKiB, uint passes, uint lanes) =>
        lanes is >= 1 and <= 16 && passes is >= 1 and <= 16 && memoryKiB >= 8 * lanes && memoryKiB <= 1048576;

    internal static StorageKeyBlock? TryParse(ReadOnlySpan<byte> bytes)
    {
        if (!StorageFormat.ValidPageSize(bytes.Length) || bytes.Length < 1024 || !bytes[..4].SequenceEqual("JEDK"u8) ||
            BinaryPrimitives.ReadUInt16BigEndian(bytes[4..]) != 1 || bytes[6] != 1 || bytes[7] is not (1 or 2) ||
            BinaryPrimitives.ReadUInt64BigEndian(bytes[8..]) == 0 || !StorageFormat.IsZero(bytes[28..32]) ||
            !StorageFormat.IsZero(bytes[128..]) || BinaryPrimitives.ReadUInt32BigEndian(bytes[124..]) != StorageFormat.Crc32(bytes[..124])) return null;
        var memory = BinaryPrimitives.ReadUInt32BigEndian(bytes[16..]);
        var passes = BinaryPrimitives.ReadUInt32BigEndian(bytes[20..]);
        var lanes = BinaryPrimitives.ReadUInt32BigEndian(bytes[24..]);
        if (bytes[7] == 1 ? !ValidArgonParameters(memory, passes, lanes) : memory != 0 || passes != 0 || lanes != 0) return null;
        return new(bytes.ToArray());
    }

    internal static StorageKeySelection Select(ReadOnlySpan<byte> page2, ReadOnlySpan<byte> page3)
    {
        var first = TryParse(page2); var second = TryParse(page3);
        if (first is null && second is null) throw StorageFormat.Corrupt("no valid key-block copy");
        if (first is null) return new(second!, 3, true);
        if (second is null) return new(first, 2, true);
        if (first.Generation == second.Generation)
        {
            if (!page2.SequenceEqual(page3)) throw StorageFormat.Corrupt("key-block copies disagree at one generation");
            return new(first, 2, false);
        }
        return first.Generation > second.Generation ? new(first, 2, true) : new(second, 3, true);
    }

    internal static StorageKeyBlock Create(int pageSize, ulong generation, ReadOnlySpan<byte> dek,
                                           StorageCredential credential, Func<int, byte[]> entropy)
    {
        CryptoProvider.CheckKey(dek);
        if (!StorageFormat.ValidPageSize(pageSize) || pageSize < 1024) throw Errors.Unsupported("encrypted page size");
        if (generation == 0) throw new ArgumentOutOfRangeException(nameof(generation));
        var bytes = new byte[pageSize];
        "JEDK"u8.CopyTo(bytes); BinaryPrimitives.WriteUInt16BigEndian(bytes.AsSpan(4), 1);
        bytes[6] = 1; bytes[7] = credential.Kind;
        BinaryPrimitives.WriteUInt64BigEndian(bytes.AsSpan(8), generation);
        BinaryPrimitives.WriteUInt32BigEndian(bytes.AsSpan(16), credential.MemoryKiB);
        BinaryPrimitives.WriteUInt32BigEndian(bytes.AsSpan(20), credential.Passes);
        BinaryPrimitives.WriteUInt32BigEndian(bytes.AsSpan(24), credential.Lanes);
        Entropy(entropy, 32).CopyTo(bytes, 32);
        Entropy(entropy, 12).CopyTo(bytes, 64);
        var kek = credential.Derive(bytes.AsSpan(32, 32), credential.MemoryKiB, credential.Passes, credential.Lanes);
        try { CryptoProvider.GcmSeal(kek, bytes.AsSpan(64, 12), dek, bytes.AsSpan(0, 76)).CopyTo(bytes, 76); }
        finally { CryptographicOperations.ZeroMemory(kek); }
        BinaryPrimitives.WriteUInt32BigEndian(bytes.AsSpan(124), StorageFormat.Crc32(bytes.AsSpan(0, 124)));
        return new(bytes);
    }

    internal byte[] Unwrap(StorageCredential? credential)
    {
        if (credential is null) throw new SqlException(SqlStates.CredentialRequired, "credential required");
        if (credential.Kind != Kind) throw new SqlException(SqlStates.InvalidPassword, "invalid credential");
        var kek = credential.Derive(bytes.AsSpan(32, 32), MemoryKiB, Passes, Lanes);
        try
        {
            return CryptoProvider.GcmOpen(kek, bytes.AsSpan(64, 12), bytes.AsSpan(76, 48), bytes.AsSpan(0, 76))
                ?? throw new SqlException(SqlStates.InvalidPassword, "invalid credential");
        }
        finally { CryptographicOperations.ZeroMemory(kek); }
    }

    internal static byte[] Entropy(Func<int, byte[]> source, int count)
    {
        var bytes = source(count);
        if (bytes is null || bytes.Length != count) throw new IOException("key entropy source returned the wrong byte count");
        return bytes;
    }
}
