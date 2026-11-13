using System.Buffers.Binary;

namespace Jed;

// Page-identical backups preserve stored ciphertext and page identities. The
// traversal reads and authenticates each reachable page once, retaining only
// the traversal stack and identity sets, never all file bytes.
internal sealed class StorageSnapshotCopy(StoragePager source, DatabaseSnapshot snapshot, IBlockStore? destination = null)
{
    private readonly HashSet<uint> seen = [];
    private uint live;
    private int Size => source.PageSize;
    private bool Encrypted => source.Encrypted;
    private int Capacity => source.PayloadCapacity;
    private int Extra => Encrypted ? 16 : 0;

    private async ValueTask<ReadOnlyMemory<byte>> ReadAsync(StoragePageReference reference, bool isLive, CancellationToken token)
    {
        if (reference.Page < (Encrypted ? 4 : 2) || reference.Page >= snapshot.Meta.PageCount || !seen.Add(reference.Page))
            throw StorageFormat.Corrupt("snapshot repeats or names an invalid page");
        var stored = new byte[Size]; await source.Store.ReadAsync((long)reference.Page * Size, stored, token).ConfigureAwait(false);
        var plain = source.Codec is null ? stored : source.Codec.OpenBody(reference.Page, stored, reference.Tag.Span);
        _ = StorageFormat.ParsePage(plain, verifyChecksum: !Encrypted);
        if (destination is not null) await destination.WriteAsync((long)reference.Page * Size, stored, token).ConfigureAwait(false);
        if (isLive) live++;
        return plain;
    }

    private async ValueTask TreeAsync(StoragePageReference reference, StorageLeafSchema schema, int depth, CancellationToken token)
    {
        if (reference.Page == 0) return;
        if (depth > 128) throw StorageFormat.Corrupt("snapshot tree exceeds depth limit");
        var page = await ReadAsync(reference, true, token).ConfigureAwait(false);
        var header = StorageFormat.ParsePage(page.Span, verifyChecksum: !Encrypted);
        if (header.Type == 3)
        {
            var interior = StorageInterior.Parse(page.Span, snapshot.Meta.PageCount, Encrypted);
            for (var i = 0; i < interior.Children.Length; i++)
                await TreeAsync(new(interior.Children[i], interior.Tags is null ? default : interior.Tags[i]), schema, depth + 1, token).ConfigureAwait(false);
        }
        else if (header.Type == 2)
        {
            var leaf = StorageLeaf.Parse(page, schema, Encrypted);
            for (var i = 0; i < leaf.Count; i++)
            {
                var row = leaf.Record(i);
                for (var column = 0; column < row.Cells.Length; column++)
                    if (schema.Columns[column].Width == 0 && StorageLargeValues.External(row.Cells[column]))
                        await OverflowAsync(row.Cells[column]!, token).ConfigureAwait(false);
            }
        }
        else throw StorageFormat.Corrupt("snapshot tree names a non-tree page");
    }

    private async ValueTask OverflowAsync(byte[] cell, CancellationToken token)
    {
        var info = StorageLargeValues.Inspect(cell, Encrypted); var remaining = info.Stored;
        var reference = new StoragePageReference(info.First, info.Tag);
        while (remaining > 0)
        {
            var page = await ReadAsync(reference, true, token).ConfigureAwait(false);
            var header = StorageFormat.ParsePage(page.Span, verifyChecksum: !Encrypted);
            var count = Math.Min(remaining, Capacity - Extra);
            if (header.Type != 4 || header.Count != count || (header.Next == 0) != (remaining == count) ||
                !StorageFormat.IsZero(page.Span.Slice(16 + Extra + count, Capacity - Extra - count)))
                throw StorageFormat.Corrupt("snapshot overflow slab is invalid");
            var tag = Encrypted ? page.Slice(16, 16).ToArray() : default;
            if (Encrypted && header.Next == 0 && !StorageFormat.IsZero(tag)) throw StorageFormat.Corrupt("nonzero snapshot overflow tail tag");
            reference = new(header.Next, tag); remaining -= count;
        }
    }

    private async ValueTask GistAsync(StoragePageReference reference, int depth, CancellationToken token)
    {
        if (reference.Page == 0) return;
        if (depth > 128) throw StorageFormat.Corrupt("GiST snapshot exceeds depth limit");
        var bytes = await ReadAsync(reference, true, token).ConfigureAwait(false);
        var page = StorageGistPage.Parse(bytes.Span, snapshot.Meta.PageCount, Encrypted);
        foreach (var child in page.Children) await GistAsync(child, depth + 1, token).ConfigureAwait(false);
    }

    internal async ValueTask<HashSet<uint>> ReachableAsync(CancellationToken token)
    {
        var reference = new StoragePageReference(snapshot.Meta.Root, snapshot.Encrypted?.RootTag ?? default);
        while (reference.Page != 0)
        {
            var page = await ReadAsync(reference, true, token).ConfigureAwait(false);
            var header = StorageFormat.ParsePage(page.Span, verifyChecksum: !Encrypted);
            if (header.Type != 1) throw StorageFormat.Corrupt("snapshot catalog names another page kind");
            var tag = Encrypted ? page.Slice(16, 16).ToArray() : default;
            if (Encrypted && header.Next == 0 && !StorageFormat.IsZero(tag)) throw StorageFormat.Corrupt("nonzero snapshot catalog tail tag");
            reference = new(header.Next, tag);
        }
        foreach (var table in snapshot.Catalog.Tables)
        {
            await TreeAsync(new(table.Root, table.RootTag), table.LeafSchema(), 0, token).ConfigureAwait(false);
            foreach (var index in table.Indexes ?? [])
            {
                if (index.Kind == 2) await GistAsync(new(index.Root, index.RootTag), 0, token).ConfigureAwait(false);
                else await TreeAsync(new(index.Root, index.RootTag), new([]), 0, token).ConfigureAwait(false);
            }
        }
        if (live != snapshot.Meta.LivePages) throw StorageFormat.Corrupt("snapshot live page count differs from reachable pages");
        return new(seen);
    }

    internal async ValueTask CopyAsync(StorageKeyBlock? keyBlock, CancellationToken token)
    {
        if (destination is null) throw new InvalidOperationException("snapshot copy has no destination");
        await destination.SetLengthAsync((long)snapshot.Meta.PageCount * Size, token).ConfigureAwait(false);
        await ReachableAsync(token).ConfigureAwait(false);
        var reference = new StoragePageReference(snapshot.Meta.FreeListHead, snapshot.Encrypted?.FreeListTag ?? default);
        var free = new HashSet<uint>(); uint previous = 0;
        while (reference.Page != 0)
        {
            var page = await ReadAsync(reference, false, token).ConfigureAwait(false);
            var header = StorageFormat.ParsePage(page.Span, verifyChecksum: !Encrypted);
            if (header.Type != 7 || header.Count > (Capacity - Extra) / 4) throw StorageFormat.Corrupt("invalid snapshot free-list page");
            for (var i = 0; i < header.Count; i++)
            {
                var id = BinaryPrimitives.ReadUInt32BigEndian(page.Span[(16 + Extra + i * 4)..]);
                if (id < (Encrypted ? 4 : 2) || id >= snapshot.Meta.PageCount || id <= previous || !free.Add(id))
                    throw StorageFormat.Corrupt("invalid snapshot free-list entry");
                previous = id;
            }
            var used = Extra + checked((int)header.Count * 4);
            if (!StorageFormat.IsZero(page.Span.Slice(16 + used, Capacity - used))) throw StorageFormat.Corrupt("nonzero snapshot free-list padding");
            var tag = Encrypted ? page.Slice(16, 16).ToArray() : default;
            if (Encrypted && header.Next == 0 && !StorageFormat.IsZero(tag)) throw StorageFormat.Corrupt("nonzero snapshot free-list tail tag");
            reference = new(header.Next, tag);
        }
        if (free.Overlaps(seen)) throw StorageFormat.Corrupt("snapshot free list includes a reachable page");
        var bootstrap = snapshot.Meta with { ManifestHead = 0, ManifestEntries = 0, ManifestPages = 0, ManifestDigest = 0 };
        if (!Encrypted)
        {
            var bytes = StorageFormat.EncodeMeta(bootstrap);
            await destination.WriteAsync(0, bytes, token).ConfigureAwait(false);
            await destination.WriteAsync(Size, bytes, token).ConfigureAwait(false);
        }
        else
        {
            if (keyBlock is null || snapshot.Encrypted is null) throw StorageFormat.Corrupt("encrypted snapshot has no key or bound roots");
            var bytes = StorageCryptoMeta.Encode(bootstrap, snapshot.Encrypted.RootTag, snapshot.Encrypted.FreeListTag, []);
            await destination.WriteAsync(2L * Size, keyBlock.Bytes, token).ConfigureAwait(false);
            await destination.WriteAsync(3L * Size, keyBlock.Bytes, token).ConfigureAwait(false);
            await destination.WriteAsync(0, source.Codec!.SealMeta(0, bytes, []), token).ConfigureAwait(false);
            await destination.WriteAsync(Size, source.Codec.SealMeta(1, bytes, []), token).ConfigureAwait(false);
        }
    }
}
