using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Jed;

internal readonly record struct StorageManifestPage(uint Id, ReadOnlyMemory<byte> Bytes);

// Cryptographic page policy above the byte device. Nonces are drawn only by seal
// operations, in the caller's specified post-order; reads consume no entropy.
internal sealed class StoragePageCodec : IDisposable
{
    internal const int Reserve = 40;
    internal const int TagLength = 16;
    internal const uint FirstBody = 4;
    private readonly Xaes256Gcm pages;
    private readonly Xaes256Gcm metas;
    private readonly Func<int, byte[]> entropy;
    private readonly byte[] dek;
    private bool disposed;

    internal StoragePageCodec(ReadOnlySpan<byte> dataKey, Func<int, byte[]> entropy)
    {
        CryptoProvider.CheckKey(dataKey);
        this.entropy = entropy;
        dek = dataKey.ToArray();
        byte[] pageKey = [], metaKey = [];
        Xaes256Gcm? pageCipher = null;
        try
        {
            pageKey = CryptoProvider.Hkdf(dek, [], "jed/v1/page"u8);
            metaKey = CryptoProvider.Hkdf(dek, [], "jed/v1/meta"u8);
            pageCipher = new Xaes256Gcm(pageKey);
            metas = new Xaes256Gcm(metaKey);
            pages = pageCipher;
        }
        catch { pageCipher?.Dispose(); CryptographicOperations.ZeroMemory(dek); throw; }
        finally { CryptographicOperations.ZeroMemory(pageKey); CryptographicOperations.ZeroMemory(metaKey); }
    }

    internal StorageKeyBlock Wrap(int pageSize, ulong generation, StorageCredential credential)
    {
        Check(); return StorageKeyBlock.Create(pageSize, generation, dek, credential, entropy);
    }

    internal byte[] SealBody(uint index, ReadOnlySpan<byte> plaintext)
    {
        Check(); CheckPageSize(plaintext.Length);
        if (index < FirstBody) throw new ArgumentOutOfRangeException(nameof(index));
        if (!StorageFormat.IsZero(plaintext[^Reserve..])) throw new ArgumentException("encrypted body page overflows its payload capacity");
        var body = plaintext[..^Reserve].ToArray();
        try
        {
            body.AsSpan(12, 4).Clear();
            ValidateBodyHeader(body);
            var nonce = StorageKeyBlock.Entropy(entropy, 24);
            var sealedBytes = pages.Seal(nonce, body, Prefix(1, index));
            var stored = new byte[plaintext.Length];
            sealedBytes.AsSpan(0, body.Length).CopyTo(stored);
            nonce.CopyTo(stored, body.Length);
            sealedBytes.AsSpan(body.Length).CopyTo(stored.AsSpan(stored.Length - TagLength));
            return stored;
        }
        finally { CryptographicOperations.ZeroMemory(body); }
    }

    internal byte[] OpenBody(uint index, ReadOnlySpan<byte> stored, ReadOnlySpan<byte> expectedTag)
    {
        Check(); CheckPageSize(stored.Length);
        if (index < FirstBody || expectedTag.Length != TagLength ||
            !CryptographicOperations.FixedTimeEquals(stored[^TagLength..], expectedTag))
            throw StorageFormat.Corrupt("encrypted page pointer tag mismatch");
        var ciphertextLength = stored.Length - Reserve;
        var sealedBytes = new byte[ciphertextLength + TagLength];
        stored[..ciphertextLength].CopyTo(sealedBytes);
        stored[^TagLength..].CopyTo(sealedBytes.AsSpan(ciphertextLength));
        var body = pages.Open(stored.Slice(ciphertextLength, 24), sealedBytes, Prefix(1, index))
            ?? throw StorageFormat.Corrupt("encrypted body page authentication failed");
        try
        {
            ValidateBodyHeader(body);
            var result = new byte[stored.Length]; body.CopyTo(result, 0); return result;
        }
        finally { CryptographicOperations.ZeroMemory(body); }
    }

    internal byte[] SealMeta(uint slot, ReadOnlySpan<byte> meta, IReadOnlyList<StorageManifestPage> manifest)
    {
        Check(); ValidateMetaShape(slot, meta);
        var result = meta.ToArray();
        var aad = MetaAssociatedData(slot, result, manifest);
        var nonce = StorageKeyBlock.Entropy(entropy, 24);
        var tag = metas.Seal(nonce, [], aad);
        nonce.CopyTo(result, result.Length - Reserve);
        tag.CopyTo(result, result.Length - TagLength);
        StorageFormat.SealMeta(result);
        return result;
    }

    // CRC/manifest candidate validation precedes this selected-meta authentication.
    // A failed tag must never trigger fallback to an older meta slot.
    internal void AuthenticateMeta(uint slot, ReadOnlySpan<byte> meta, IReadOnlyList<StorageManifestPage> manifest)
    {
        Check(); ValidateMetaShape(slot, meta);
        var aad = MetaAssociatedData(slot, meta, manifest);
        if (metas.Open(meta.Slice(meta.Length - Reserve, 24), meta[^TagLength..], aad) is null)
            throw StorageFormat.Corrupt("encrypted meta authentication failed");
    }

    private static byte[] MetaAssociatedData(uint slot, ReadOnlySpan<byte> meta, IReadOnlyList<StorageManifestPage> manifest)
    {
        var length = 10L + meta.Length;
        foreach (var page in manifest)
        {
            if (page.Id < FirstBody || page.Bytes.Length != meta.Length) throw StorageFormat.Corrupt("invalid manifest authentication input");
            length = checked(length + 4 + page.Bytes.Length);
        }
        if (length > int.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "meta authentication input is too large");
        var aad = new byte[(int)length];
        Prefix(2, slot).CopyTo(aad, 0); meta.CopyTo(aad.AsSpan(10));
        aad.AsSpan(10 + 32, 4).Clear(); aad.AsSpan(10 + meta.Length - Reserve, Reserve).Clear();
        var position = 10 + meta.Length;
        foreach (var page in manifest)
        {
            BinaryPrimitives.WriteUInt32BigEndian(aad.AsSpan(position), page.Id); position += 4;
            page.Bytes.Span.CopyTo(aad.AsSpan(position)); position += page.Bytes.Length;
        }
        return aad;
    }

    private static byte[] Prefix(byte role, uint index)
    {
        var bytes = new byte[10]; "JEDB"u8.CopyTo(bytes); bytes[4] = 1; bytes[5] = role;
        BinaryPrimitives.WriteUInt32BigEndian(bytes.AsSpan(6), index); return bytes;
    }
    private static void ValidateBodyHeader(ReadOnlySpan<byte> bytes)
    {
        var type = bytes[0]; var flags = bytes[1];
        if (type is < 1 or > 7 || bytes[2] != 0 || bytes[3] != 0 || !StorageFormat.IsZero(bytes[12..16]) ||
            (type == 1 ? (flags & ~1) != 0 : flags != 0) ||
            (type is 2 or 3 or 5 or 6) && BinaryPrimitives.ReadUInt32BigEndian(bytes[8..]) != 0)
            throw StorageFormat.Corrupt("invalid decrypted body header");
    }
    private static void ValidateMetaShape(uint slot, ReadOnlySpan<byte> meta)
    {
        CheckPageSize(meta.Length);
        if (slot > 1 || !meta[..4].SequenceEqual("JEDB"u8) || meta[6] != 1 || meta[7] != Reserve)
            throw StorageFormat.Corrupt("invalid encrypted meta shape");
    }
    private static void CheckPageSize(int size)
    {
        if (!StorageFormat.ValidPageSize(size) || size < 1024) throw StorageFormat.Corrupt("invalid encrypted page size");
    }
    private void Check() => ObjectDisposedException.ThrowIf(disposed, this);
    public void Dispose()
    {
        if (disposed) return;
        disposed = true; pages.Dispose(); metas.Dispose(); CryptographicOperations.ZeroMemory(dek);
    }
}
