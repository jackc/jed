using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Jed;

public sealed partial class Database
{
    private static byte[][] CreateEncryptedImage(int pageSize, DatabaseFileOptions options)
    {
        var key = StorageKeyBlock.Entropy(options.KeyEntropy, 32);
        try
        {
            using var codec = new StoragePageCodec(key, options.KeyEntropy);
            var block = codec.Wrap(pageSize, 1, options.Credential!.Value).Bytes.ToArray();
            var root = codec.SealBody(4, StorageFormat.EncodePage(pageSize, new(1, 0, 0, 0), new byte[16]));
            var meta = StorageCryptoMeta.Encode(new(pageSize, 1, 4, 5, 0, 1), root.AsSpan(pageSize - 16), new byte[16], []);
            var first = codec.SealMeta(0, meta, []); var second = codec.SealMeta(1, meta, []);
            return [first, second, block, block, root];
        }
        finally { CryptographicOperations.ZeroMemory(key); }
    }
    private sealed record OpenedStorage(StoragePager Pager, StorageRecovered Recovered, Catalog Catalog, StorageKeySelection? KeyBlock);
    private sealed record ServedStorage(StorageMeta Meta, StorageEncryptedMeta? Encrypted, StoragePageCodec? Codec, StorageKeySelection? KeyBlock);

    private static async ValueTask<byte> InspectVariantAsync(IBlockStore store, CancellationToken token)
    {
        var length = await store.GetLengthAsync(token).ConfigureAwait(false);
        if (length < 12) throw StorageFormat.Corrupt("database file smaller than meta header");
        var header = new byte[12]; await store.ReadAsync(0, header, token).ConfigureAwait(false);
        var size = BinaryPrimitives.ReadUInt32BigEndian(header.AsSpan(8));
        if (size > int.MaxValue || !StorageFormat.ValidPageSize((int)size) || length < 2L * size) throw StorageFormat.Corrupt("invalid database page size");
        var candidates = new List<(StorageMeta Meta, byte Variant, byte[] Bytes)>();
        for (var slot = 0; slot < 2; slot++)
        {
            var bytes = new byte[(int)size]; await store.ReadAsync(slot * (long)size, bytes, token).ConfigureAwait(false);
            if (StorageFormat.ParseMeta(bytes, length) is { } plain) candidates.Add((plain, 0, bytes));
            else if (StorageCryptoMeta.Parse(bytes, length) is { } encrypted) candidates.Add((encrypted.Header, 1, bytes));
        }
        foreach (var candidate in candidates.OrderByDescending(candidate => candidate.Meta.TransactionId))
        {
            var complete = candidate.Variant == 0
                ? await StorageRecovery.ValidateManifestAsync(store, candidate.Meta, candidate.Bytes, token).ConfigureAwait(false) is not null
                : await StorageCryptoRecovery.ValidateManifestAsync(store, candidate.Meta, candidate.Bytes, token).ConfigureAwait(false) is not null;
            if (complete) return candidate.Variant;
        }
        throw StorageFormat.Corrupt("no complete database meta candidate");
    }

    private static async ValueTask<OpenedStorage> OpenFileStorageAsync(IBlockStore store, DatabaseFileOptions options,
        StoragePageCodec? heldCodec, CancellationToken token)
    {
        if (await InspectVariantAsync(store, token).ConfigureAwait(false) == 0)
        {
            if (heldCodec is not null) throw new SqlException(SqlStates.StaleKey, "encrypted database was replaced by plaintext; reopen it");
            if (options.Credential is not null) throw new SqlException(SqlStates.CredentialNotApplicable, "credential does not apply to a plaintext file");
            var recovered = await StorageRecovery.OpenAsync(store, token).ConfigureAwait(false);
            var pager = new StoragePager(store, recovered.Meta.PageSize);
            return new(pager, recovered, await Catalog.ReadAsync(pager, recovered.Meta, token).ConfigureAwait(false), null);
        }
        var encrypted = await StorageCryptoRecovery.OpenAsync(store, options.Credential?.Value, options.KeyEntropy, token, heldCodec).ConfigureAwait(false);
        try
        {
            var pager = new StoragePager(store, encrypted.Meta.Header.PageSize, codec: encrypted.Codec);
            return new(pager, encrypted.Recovered, await Catalog.ReadAsync(pager, encrypted.Meta, token).ConfigureAwait(false), encrypted.KeyBlock);
        }
        catch { encrypted.Dispose(); throw; }
    }

    private static async ValueTask<ServedStorage> UnlockClientAsync(IBlockStore store, byte[] rawMeta, DatabaseFileOptions options,
        StoragePageCodec? heldCodec, bool alreadyOpened, CancellationToken token)
    {
        var length = await store.GetLengthAsync(token).ConfigureAwait(false);
        if (rawMeta[6] == 0)
        {
            if (heldCodec is not null) throw new SqlException(SqlStates.StaleKey, "encrypted database was replaced by plaintext; reopen it");
            var plain = StorageFormat.ParseMeta(rawMeta, length) ?? throw StorageFormat.Corrupt("invalid served meta");
            if (!alreadyOpened && options.Credential is not null) throw new SqlException(SqlStates.CredentialNotApplicable, "credential does not apply to a plaintext file");
            return new(plain, null, null, null);
        }
        var meta = StorageCryptoMeta.Parse(rawMeta, length) ?? throw StorageFormat.Corrupt("invalid encrypted served meta");
        if (alreadyOpened && heldCodec is null) throw new SqlException(SqlStates.CredentialRequired, "database was encrypted; reopen with a credential");
        var key2 = new byte[meta.Header.PageSize]; var key3 = new byte[meta.Header.PageSize];
        await store.ReadAsync(2L * key2.Length, key2, token).ConfigureAwait(false);
        await store.ReadAsync(3L * key3.Length, key3, token).ConfigureAwait(false);
        var selected = StorageKeyBlock.Select(key2, key3); var codec = heldCodec;
        if (codec is null)
        {
            var dataKey = selected.Block.Unwrap(options.Credential?.Value);
            try { codec = new(dataKey, options.KeyEntropy); }
            finally { CryptographicOperations.ZeroMemory(dataKey); }
        }
        try
        {
            // The service owns durable-manifest validation. Its client reads
            // only the manifest chain required as meta AEAD associated data.
            var manifest = await StorageCryptoRecovery.ValidateManifestAsync(store, meta.Header, rawMeta, token, readBodies: false).ConfigureAwait(false)
                ?? throw StorageFormat.Corrupt("invalid served manifest chain");
            AuthenticateServed(codec, rawMeta, manifest.Pages, heldCodec is not null);
            return new(meta.Header, meta, codec, selected);
        }
        catch { if (heldCodec is null) codec.Dispose(); throw; }
    }
    private static void AuthenticateServed(StoragePageCodec codec, byte[] meta, IReadOnlyList<StorageManifestPage> pages, bool held)
    {
        try { codec.AuthenticateMeta(0, meta, pages); return; }
        catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { }
        try { codec.AuthenticateMeta(1, meta, pages); }
        catch (SqlException error) when (held && error.SqlState == SqlStates.DataCorrupted)
        { throw new SqlException(SqlStates.StaleKey, "database data key changed; reopen with its current credential"); }
    }
    private static ValueTask<Catalog> ReadServedCatalogAsync(StoragePager pager, ServedStorage served, CancellationToken token) =>
        served.Encrypted is { } encrypted ? Catalog.ReadAsync(pager, encrypted, token) : Catalog.ReadAsync(pager, served.Meta, token);
}
