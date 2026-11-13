using System.Buffers.Binary;

namespace Jed;

// A from-scratch sink keeps no database-sized collection of page buffers.
// Allocation order is independent of emission order: bound children and chain
// successors are sealed before their parents even when their IDs are higher.
internal sealed class StorageImageWriter(IBlockStore destination, int pageSize, StoragePageCodec? codec = null)
{
    private uint next = codec is null ? 2u : 4u;
    internal int PageSize => pageSize;
    internal bool Encrypted => codec is not null;
    internal int PayloadCapacity => pageSize - 16 - (Encrypted ? StoragePageCodec.Reserve : 0);
    internal uint Reserve(int count = 1)
    {
        if (count < 1 || (ulong)next + (uint)count > uint.MaxValue)
            throw new SqlException(SqlStates.ProgramLimitExceeded, "database image exceeds page address space");
        var first = next; next += (uint)count; return first;
    }
    internal async ValueTask<StoragePageReference> WriteAsync(uint id, byte[] page, CancellationToken token)
    {
        if (id < (Encrypted ? 4u : 2u) || id >= next || page.Length != pageSize) throw new ArgumentException("invalid image page");
        var stored = codec is null ? page : codec.SealBody(id, page);
        await destination.WriteAsync((long)id * pageSize, stored, token).ConfigureAwait(false);
        return new(id, Encrypted ? stored.AsMemory(pageSize - 16).ToArray() : default);
    }
    internal async ValueTask<StoragePageReference> CatalogAsync(Catalog catalog, CancellationToken token)
    {
        var chunks = catalog.ImageChunks(PayloadCapacity - (Encrypted ? 16 : 0), Encrypted);
        var first = Reserve(chunks.Count); var successor = new StoragePageReference(0, new byte[16]);
        for (var i = chunks.Count - 1; i >= 0; i--)
        {
            var chunk = chunks[i]; var payload = new byte[chunk.Payload.Length + (Encrypted ? 16 : 0)];
            if (Encrypted) successor.Tag.Span.CopyTo(payload);
            chunk.Payload.CopyTo(payload, Encrypted ? 16 : 0);
            successor = await WriteAsync(first + (uint)i, StorageFormat.EncodePage(pageSize,
                new(1, chunk.Flags, chunk.Count, successor.Page), payload), token).ConfigureAwait(false);
        }
        return successor;
    }
    internal async ValueTask<StorageMeta> FinishAsync(StoragePageReference root, ulong transactionId,
        StorageKeyBlock? keyBlock, CancellationToken token)
    {
        if (Encrypted != (keyBlock is not null)) throw new ArgumentException("image key block differs from codec");
        var meta = new StorageMeta(pageSize, transactionId, root.Page, next, 0, next - (Encrypted ? 4u : 2u));
        if (codec is null)
        {
            var bytes = StorageFormat.EncodeMeta(meta);
            await destination.WriteAsync(0, bytes, token).ConfigureAwait(false);
            await destination.WriteAsync(pageSize, bytes, token).ConfigureAwait(false);
        }
        else
        {
            var header = StorageCryptoMeta.Encode(meta, root.Tag.Span, new byte[16], []);
            await destination.WriteAsync(2L * pageSize, keyBlock!.Bytes, token).ConfigureAwait(false);
            await destination.WriteAsync(3L * pageSize, keyBlock.Bytes, token).ConfigureAwait(false);
            await destination.WriteAsync(0, codec.SealMeta(0, header, []), token).ConfigureAwait(false);
            await destination.WriteAsync(pageSize, codec.SealMeta(1, header, []), token).ConfigureAwait(false);
        }
        await destination.SetLengthAsync((long)next * pageSize, token).ConfigureAwait(false);
        return meta;
    }
    internal async ValueTask<byte[]> CopyChainAsync(byte[] pointer, StoragePager source, uint sourcePageCount, CancellationToken token)
    {
        if (source.Encrypted != Encrypted || source.PageSize != pageSize) throw new ArgumentException("image copy requires the same page layout");
        var refs = new List<StoragePageReference>();
        // Retain only identities and copied tags, not page-backed tag slices:
        // an arbitrarily long value must not retain the pager's whole contents.
        await StorageLargeValues.WalkBoundChainAsync(pointer, source, sourcePageCount,
            (reference, _) => refs.Add(new(reference.Page, reference.Tag.ToArray())), token).ConfigureAwait(false);
        var first = Reserve(refs.Count); var successor = new StoragePageReference(0, new byte[16]);
        for (var i = refs.Count - 1; i >= 0; i--)
        {
            var original = await source.ReadPageAsync(refs[i], token).ConfigureAwait(false);
            var header = StorageFormat.ParsePage(original.Span, verifyChecksum: !Encrypted);
            var payload = new byte[checked((int)header.Count) + (Encrypted ? 16 : 0)];
            if (Encrypted) successor.Tag.Span.CopyTo(payload);
            original.Slice(Encrypted ? 32 : 16, (int)header.Count).Span.CopyTo(payload.AsSpan(Encrypted ? 16 : 0));
            successor = await WriteAsync(first + (uint)i, StorageFormat.EncodePage(pageSize,
                new(4, 0, header.Count, successor.Page), payload), token).ConfigureAwait(false);
        }
        var copied = pointer.ToArray(); BinaryPrimitives.WriteUInt32BigEndian(copied.AsSpan(1), first);
        if (Encrypted) successor.Tag.Span.CopyTo(copied.AsSpan(copied.Length - 16));
        return copied;
    }
}

internal sealed partial class StorageTree
{
    internal async ValueTask<StoragePageReference> CopyImageAsync(StorageImageWriter output, CancellationToken token)
    {
        if (output.PageSize != pager.PageSize || output.Encrypted != pager.Encrypted) throw new ArgumentException("image copy requires the same page layout");
        var seen = new HashSet<uint>();
        async ValueTask<StoragePageReference> Copy(Ref reference, int depth)
        {
            CheckPath(reference, seen, depth);
            var node = await Load(reference, token).ConfigureAwait(false);
            if (node.IsLeaf)
            {
                var id = output.Reserve(); var rows = node.Records().Select(Clone).ToArray();
                foreach (var row in rows)
                    for (var column = 0; column < row.Cells.Length; column++)
                        if (schema.Columns[column].Width == 0 && StorageLargeValues.External(row.Cells[column]))
                            row.Cells[column] = await output.CopyChainAsync(row.Cells[column]!, pager, pageCount, token).ConfigureAwait(false);
                return await output.WriteAsync(id, StorageLeaf.Encode(pager.PageSize, schema, rows, pager.Encrypted), token).ConfigureAwait(false);
            }
            var children = new uint[node.Children!.Length]; var tags = pager.Encrypted ? new byte[children.Length][] : null;
            for (var i = 0; i < children.Length; i++)
            {
                var child = await Copy(node.Children[i], depth + 1).ConfigureAwait(false); children[i] = child.Page;
                if (tags is not null) tags[i] = child.Tag.ToArray();
            }
            return await output.WriteAsync(output.Reserve(), new StorageInterior(children, node.Keys!) { Tags = tags }
                .Encode(pager.PageSize, pager.Encrypted), token).ConfigureAwait(false);
        }
        return root is null ? new(0, pager.Encrypted ? new byte[16] : default) : await Copy(root, 0).ConfigureAwait(false);
    }
}
