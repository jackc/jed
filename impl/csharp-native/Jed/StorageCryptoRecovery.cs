using System.Buffers.Binary;
using System.Security.Cryptography;

namespace Jed;

internal sealed record StorageEncryptedRecovery(StorageRecovered Recovered, StorageEncryptedMeta Meta,
    byte[] MetaBytes, StorageKeySelection KeyBlock, StoragePageCodec Codec) : IDisposable
{
    internal bool OwnsCodec { get; init; } = true;
    public void Dispose() { if (OwnsCodec) Codec.Dispose(); }
}

// Selection remains keyless: only a complete stored-byte manifest can nominate
// a meta. Authentication follows selection, so tampering cannot roll back a
// database by making its newest complete generation fail authentication.
internal static class StorageCryptoRecovery
{
    internal static async ValueTask<StorageEncryptedRecovery> OpenAsync(IBlockStore store, StorageCredential? credential,
        Func<int, byte[]> entropy, CancellationToken cancellationToken = default, StoragePageCodec? heldCodec = null)
    {
        var length = await store.GetLengthAsync(cancellationToken).ConfigureAwait(false);
        if (length < 12) throw StorageFormat.Corrupt("database file smaller than meta header");
        var header = new byte[12]; await store.ReadAsync(0, header, cancellationToken).ConfigureAwait(false);
        var rawSize = BinaryPrimitives.ReadUInt32BigEndian(header.AsSpan(8));
        if (rawSize > int.MaxValue || !StorageFormat.ValidPageSize((int)rawSize) || rawSize < 1024 || length < 4L * rawSize)
            throw StorageFormat.Corrupt("invalid encrypted page size");
        var size = (int)rawSize; var blocks = new[] { new byte[size], new byte[size] }; var metas = new StorageEncryptedMeta?[2];
        for (var slot = 0; slot < 2; slot++)
        {
            await store.ReadAsync((long)slot * size, blocks[slot], cancellationToken).ConfigureAwait(false);
            metas[slot] = StorageCryptoMeta.Parse(blocks[slot], length);
        }
        int[] order = metas[1] is not null && (metas[0] is null || metas[1]!.Header.TransactionId > metas[0]!.Header.TransactionId) ? [1, 0] : [0, 1];
        foreach (var slot in order)
        {
            if (metas[slot] is not { } meta) continue;
            var manifest = await ValidateManifestAsync(store, meta.Header, blocks[slot], cancellationToken).ConfigureAwait(false);
            if (manifest is null) continue;
            var key2 = new byte[size]; var key3 = new byte[size];
            await store.ReadAsync(2L * size, key2, cancellationToken).ConfigureAwait(false);
            await store.ReadAsync(3L * size, key3, cancellationToken).ConfigureAwait(false);
            var key = StorageKeyBlock.Select(key2, key3);
            StoragePageCodec codec;
            if (heldCodec is null)
            {
                var dataKey = key.Block.Unwrap(credential);
                try { codec = new(dataKey, entropy); }
                finally { CryptographicOperations.ZeroMemory(dataKey); }
            }
            else codec = heldCodec;
            try
            {
                try { codec.AuthenticateMeta((uint)slot, blocks[slot], manifest.Pages); }
                catch (SqlException error) when (heldCodec is not null && error.SqlState == SqlStates.DataCorrupted)
                { throw new SqlException(SqlStates.StaleKey, "database data key changed; reopen with its current credential"); }
                var floor = slot;
                if (metas[1 - slot] is { } sibling && sibling.Header.TransactionId == meta.Header.TransactionId)
                {
                    var other = await ValidateManifestAsync(store, sibling.Header, blocks[1 - slot], cancellationToken).ConfigureAwait(false);
                    if (other is not null)
                    {
                        try { codec.AuthenticateMeta((uint)(1 - slot), blocks[1 - slot], other.Pages); floor = (int)(meta.Header.TransactionId & 1); }
                        catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { }
                    }
                }
                var (free, chain) = await ReadFreeListAsync(store, meta, codec, manifest.Dependencies, cancellationToken).ConfigureAwait(false);
                var recovered = new StorageRecovered(meta.Header, slot, floor, manifest.Dependencies, free, chain)
                { ManifestPages = manifest.Pages.Select(page => page.Id).ToHashSet(), Encrypted = meta };
                return new(recovered, meta, blocks[slot], key, codec) { OwnsCodec = heldCodec is null };
            }
            catch { if (heldCodec is null) codec.Dispose(); throw; }
        }
        throw StorageFormat.Corrupt("no complete encrypted meta page and commit manifest");
    }

    internal sealed record Manifest(HashSet<uint> Dependencies, StorageManifestPage[] Pages);
    internal static async ValueTask<Manifest?> ValidateManifestAsync(IBlockStore store, StorageMeta meta,
        ReadOnlyMemory<byte> rawMeta, CancellationToken cancellationToken = default, bool readBodies = true)
    {
        var dependencies = new HashSet<uint>(); var pages = new List<StorageManifestPage>();
        var body = new byte[meta.PageSize]; uint previous = 3, seen = 0;
        var rootSeen = false; var freeSeen = meta.FreeListHead == 0;
        bool Pointer(uint id) => id >= 4 && id < meta.PageCount;
        async ValueTask<bool> Entries(ReadOnlyMemory<byte> bytes, uint count)
        {
            for (var index = 0; index < count; index++)
            {
                var entry = StorageFormat.ReadEntry(bytes.Span.Slice(index * 12, 12));
                if (entry.Page <= previous || !Pointer(entry.Page) || !dependencies.Add(entry.Page)) return false;
                previous = entry.Page; rootSeen |= entry.Page == meta.Root; freeSeen |= entry.Page == meta.FreeListHead;
                if (readBodies)
                {
                    await store.ReadAsync((long)entry.Page * meta.PageSize, body, cancellationToken).ConfigureAwait(false);
                    if (StorageFormat.Crc64(body) != entry.Digest) return false;
                }
                seen++;
            }
            return true;
        }
        if (!await Entries(rawMeta[96..], Math.Min(meta.ManifestEntries, (uint)StorageCryptoMeta.InlineCapacity(meta.PageSize))).ConfigureAwait(false)) return null;
        var id = meta.ManifestHead; ulong digest = 0; var idBytes = new byte[4];
        if ((long)meta.ManifestPages * (meta.PageSize + 4L) + meta.PageSize + 10 > int.MaxValue)
            throw new SqlException(SqlStates.ProgramLimitExceeded, "encrypted manifest authentication input is too large");
        for (uint ordinal = 0; ordinal < meta.ManifestPages; ordinal++)
        {
            if (!Pointer(id) || !dependencies.Add(id)) return null;
            var bytes = new byte[meta.PageSize]; await store.ReadAsync((long)id * meta.PageSize, bytes, cancellationToken).ConfigureAwait(false);
            StoragePageHeader page;
            try { page = StorageFormat.ParsePage(bytes); }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { return null; }
            var count = Math.Min(meta.ManifestEntries - seen, (uint)StorageFormat.OverflowCapacity(meta.PageSize));
            if (page.Type != 8 || page.Count != count || BinaryPrimitives.ReadUInt64BigEndian(bytes.AsSpan(16)) != meta.TransactionId ||
                BinaryPrimitives.ReadUInt32BigEndian(bytes.AsSpan(24)) != ordinal || !StorageFormat.IsZero(bytes.AsSpan(28, 4)) ||
                !StorageFormat.IsZero(bytes.AsSpan(32 + (int)count * 12))) return null;
            BinaryPrimitives.WriteUInt32BigEndian(idBytes, id); digest = StorageFormat.Crc64(bytes, StorageFormat.Crc64(idBytes, digest));
            pages.Add(new(id, bytes));
            if (!await Entries(bytes.AsMemory(32), count).ConfigureAwait(false)) return null;
            id = page.Next;
        }
        return id == 0 && seen == meta.ManifestEntries && digest == meta.ManifestDigest && (seen == 0 || rootSeen && freeSeen)
            ? new(dependencies, pages.ToArray()) : null;
    }

    internal static async ValueTask<(SortedSet<uint> Free, HashSet<uint> Chain)> ReadFreeListAsync(IBlockStore store,
        StorageEncryptedMeta meta, StoragePageCodec codec, HashSet<uint> dependencies, CancellationToken cancellationToken = default)
    {
        var header = meta.Header; var free = new SortedSet<uint>(); var chain = new HashSet<uint>();
        var stored = new byte[header.PageSize]; var id = header.FreeListHead; var tag = meta.FreeListTag; uint previous = 3;
        while (id != 0)
        {
            if (id < 4 || id >= header.PageCount || !chain.Add(id) || free.Contains(id) || id == header.Root)
                throw StorageFormat.Corrupt("invalid encrypted free-list chain");
            await store.ReadAsync((long)id * header.PageSize, stored, cancellationToken).ConfigureAwait(false);
            var bytes = codec.OpenBody(id, stored, tag); var page = StorageFormat.ParsePage(bytes, verifyChecksum: false);
            if (page.Type != 7 || page.Count == 0 || page.Count > (header.PageSize - 32 - StoragePageCodec.Reserve) / 4 ||
                !StorageFormat.IsZero(bytes.AsSpan(32 + (int)page.Count * 4)) || page.Next == 0 && !StorageFormat.IsZero(bytes.AsSpan(16, 16)))
                throw StorageFormat.Corrupt("invalid encrypted free-list page");
            for (var i = 0; i < page.Count; i++)
            {
                var entry = BinaryPrimitives.ReadUInt32BigEndian(bytes.AsSpan(32 + i * 4));
                if (entry <= previous || entry < 4 || entry >= header.PageCount || dependencies.Contains(entry) || chain.Contains(entry) || entry == header.Root)
                    throw StorageFormat.Corrupt("invalid encrypted free-list entry");
                free.Add(entry); previous = entry;
            }
            id = page.Next; tag = bytes.AsSpan(16, 16).ToArray();
        }
        return (free, chain);
    }
}
