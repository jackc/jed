using System.Buffers.Binary;

namespace Jed;

internal sealed record StorageRecovered(StorageMeta Meta, int Slot, int FloorSlot,
    HashSet<uint> ProtectedPages, SortedSet<uint> FreePages, HashSet<uint> FreeListPages)
{
    internal HashSet<uint> ManifestPages { get; init; } = [];
    internal StorageEncryptedMeta? Encrypted { get; init; }
    internal StorageBatch? Batch { get; init; }
    internal uint? LastReclamationLive { get; init; }
    internal ulong? ReclamationTransactionId { get; init; }
    internal ulong FreeGeneration => ReclamationTransactionId ?? Meta.TransactionId;
    internal uint ReclamationLive => LastReclamationLive ?? checked(Meta.PageCount - (uint)FreePages.Count);
    internal bool RequiresStabilization => Meta.ManifestEntries != 0;
}

internal static class StorageRecovery
{
    internal static async ValueTask<StorageRecovered> OpenAsync(IBlockStore store, CancellationToken cancellationToken = default)
    {
        var length = await store.GetLengthAsync(cancellationToken).ConfigureAwait(false);
        if (length < 12) throw StorageFormat.Corrupt("database file smaller than meta header");
        var header = new byte[12];
        await store.ReadAsync(0, header, cancellationToken).ConfigureAwait(false);
        var rawSize = BinaryPrimitives.ReadUInt32BigEndian(header.AsSpan(8));
        if (rawSize > int.MaxValue || !StorageFormat.ValidPageSize((int)rawSize))
            throw StorageFormat.Corrupt("invalid page size in meta header");
        var size = (int)rawSize;
        if (length < 2L * size) throw StorageFormat.Corrupt("database file smaller than two meta pages");
        var blocks = new[] { new byte[size], new byte[size] };
        var metas = new StorageMeta?[2];
        for (var slot = 0; slot < 2; slot++)
        {
            await store.ReadAsync((long)slot * size, blocks[slot], cancellationToken).ConfigureAwait(false);
            metas[slot] = StorageFormat.ParseMeta(blocks[slot], length);
        }
        // Invalid candidates can fall back; a device read error must never do so.
        int[] order = metas[1] is not null && (metas[0] is null || metas[1]!.TransactionId > metas[0]!.TransactionId) ? [1, 0] : [0, 1];
        foreach (var slot in order)
        {
            var meta = metas[slot];
            if (meta is null) continue;
            var manifestPages = new HashSet<uint>();
            var dependencies = await ValidateManifestAsync(store, meta, blocks[slot], cancellationToken, manifestPages).ConfigureAwait(false);
            if (dependencies is null) continue;
            var floor = slot;
            if (metas[1 - slot] is { } sibling && sibling.TransactionId == meta.TransactionId &&
                await ValidateManifestAsync(store, sibling, blocks[1 - slot], cancellationToken).ConfigureAwait(false) is not null)
                floor = (int)(meta.TransactionId & 1);
            var (free, chain) = await ReadFreeListAsync(store, meta, dependencies, cancellationToken).ConfigureAwait(false);
            return new(meta, slot, floor, dependencies, free, chain) { ManifestPages = manifestPages };
        }
        throw StorageFormat.Corrupt("no complete meta page and commit manifest");
    }

    // Retain only page identities, never a whole-file or whole-dirty-set byte buffer.
    internal static async ValueTask<HashSet<uint>?> ValidateManifestAsync(IBlockStore store, StorageMeta meta,
        ReadOnlyMemory<byte> rawMeta, CancellationToken cancellationToken = default, HashSet<uint>? manifestPages = null)
    {
        var dependencies = new HashSet<uint>();
        var body = new byte[meta.PageSize];
        uint last = 1; uint seen = 0;
        var rootSeen = false; var freeSeen = meta.FreeListHead == 0;
        async ValueTask<bool> ValidateEntries(ReadOnlyMemory<byte> entries, uint count)
        {
            for (var i = 0; i < count; i++)
            {
                var entry = StorageFormat.ReadEntry(entries.Span.Slice(i * 12, 12));
                if (entry.Page <= last || !StorageFormat.BodyPointer(entry.Page, meta.PageCount) || !dependencies.Add(entry.Page)) return false;
                last = entry.Page;
                rootSeen |= entry.Page == meta.Root; freeSeen |= entry.Page == meta.FreeListHead;
                await store.ReadAsync((long)entry.Page * meta.PageSize, body, cancellationToken).ConfigureAwait(false);
                if (StorageFormat.Crc64(body) != entry.Digest) return false;
                try { if (StorageFormat.ParsePage(body).Type == 8) return false; }
                catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { return false; }
                seen++;
            }
            return true;
        }

        var inline = Math.Min(meta.ManifestEntries, (uint)StorageFormat.InlineCapacity(meta.PageSize));
        if (!await ValidateEntries(rawMeta[64..], inline).ConfigureAwait(false)) return null;
        var id = meta.ManifestHead; ulong digest = 0;
        var overflow = new byte[meta.PageSize]; var idBytes = new byte[4];
        for (uint ordinal = 0; ordinal < meta.ManifestPages; ordinal++)
        {
            if (!StorageFormat.BodyPointer(id, meta.PageCount) || !dependencies.Add(id)) return null;
            manifestPages?.Add(id);
            await store.ReadAsync((long)id * meta.PageSize, overflow, cancellationToken).ConfigureAwait(false);
            StoragePageHeader page;
            try { page = StorageFormat.ParsePage(overflow); }
            catch (SqlException error) when (error.SqlState == SqlStates.DataCorrupted) { return null; }
            var count = Math.Min(meta.ManifestEntries - seen, (uint)StorageFormat.OverflowCapacity(meta.PageSize));
            if (page.Type != 8 || page.Count != count ||
                BinaryPrimitives.ReadUInt64BigEndian(overflow.AsSpan(16)) != meta.TransactionId ||
                BinaryPrimitives.ReadUInt32BigEndian(overflow.AsSpan(24)) != ordinal ||
                !StorageFormat.IsZero(overflow.AsSpan(28, 4)) || !StorageFormat.IsZero(overflow.AsSpan(32 + (int)count * 12))) return null;
            BinaryPrimitives.WriteUInt32BigEndian(idBytes, id);
            digest = StorageFormat.Crc64(overflow, StorageFormat.Crc64(idBytes, digest));
            if (!await ValidateEntries(overflow.AsMemory(32), count).ConfigureAwait(false)) return null;
            id = page.Next;
        }
        return id == 0 && seen == meta.ManifestEntries && digest == meta.ManifestDigest &&
            (seen == 0 || (rootSeen && freeSeen)) ? dependencies : null;
    }

    internal static async ValueTask<(SortedSet<uint>, HashSet<uint>)> ReadFreeListAsync(IBlockStore store, StorageMeta meta,
        HashSet<uint> dependencies, CancellationToken cancellationToken)
    {
        var free = new SortedSet<uint>(); var chain = new HashSet<uint>();
        var block = new byte[meta.PageSize];
        var id = meta.FreeListHead; uint previous = 1;
        while (id != 0)
        {
            if (!StorageFormat.BodyPointer(id, meta.PageCount) || !chain.Add(id) || free.Contains(id) || id == meta.Root)
                throw StorageFormat.Corrupt("invalid free-list chain");
            await store.ReadAsync((long)id * meta.PageSize, block, cancellationToken).ConfigureAwait(false);
            var page = StorageFormat.ParsePage(block);
            if (page.Type != 7 || page.Count > (meta.PageSize - 16) / 4 ||
                !StorageFormat.IsZero(block.AsSpan(16 + (int)page.Count * 4)))
                throw StorageFormat.Corrupt($"invalid free-list page {id} (type={page.Type}, count={page.Count}, size={meta.PageSize})");
            for (var i = 0; i < page.Count; i++)
            {
                var entry = BinaryPrimitives.ReadUInt32BigEndian(block.AsSpan(16 + i * 4));
                if (entry <= previous || !StorageFormat.BodyPointer(entry, meta.PageCount) ||
                    dependencies.Contains(entry) || chain.Contains(entry) || entry == meta.Root)
                    throw StorageFormat.Corrupt("invalid free-list entry");
                free.Add(entry); previous = entry;
            }
            id = page.Next;
        }
        return (free, chain);
    }
}
