using System.Buffers.Binary;

namespace Jed;

// Structural decoding for copying the persisted R-tree. SQL GiST construction
// and semantic bounding predicates remain independent implementation work.
internal sealed record StorageGistPage(StoragePageHeader Header, int Used, int[] ChildOffsets, StoragePageReference[] Children)
{
    internal static StorageGistPage Parse(ReadOnlySpan<byte> page, uint pageCount, bool encrypted)
    {
        var header = StorageFormat.ParsePage(page, verifyChecksum: !encrypted);
        if (header.Type is not (5 or 6) || header.Next != 0 || header.Count is < 1 or > 4)
            throw StorageFormat.Corrupt("invalid GiST page header");
        var end = page.Length - (encrypted ? StoragePageCodec.Reserve : 0); var at = 16;
        var offsets = new List<int>(); var children = new List<StoragePageReference>();
        for (var i = 0; i < header.Count; i++)
        {
            if (at > end - 2) throw StorageFormat.Corrupt("truncated GiST bound length");
            var length = BinaryPrimitives.ReadUInt16BigEndian(page[at..]); at += 2;
            if (length == 0 || length > end - at) throw StorageFormat.Corrupt("invalid GiST bound length");
            at += length;
            if (header.Type == 5)
            {
                if (at > end - 2) throw StorageFormat.Corrupt("truncated GiST storage-key length");
                length = BinaryPrimitives.ReadUInt16BigEndian(page[at..]); at += 2;
                if (length == 0 || length > end - at) throw StorageFormat.Corrupt("invalid GiST storage-key length");
                at += length;
            }
            else
            {
                var size = encrypted ? 20 : 4;
                if (at > end - size) throw StorageFormat.Corrupt("truncated GiST child");
                var child = BinaryPrimitives.ReadUInt32BigEndian(page[at..]);
                if (child < (encrypted ? 4 : 2) || child >= pageCount) throw StorageFormat.Corrupt("GiST child out of bounds");
                offsets.Add(at); children.Add(new(child, encrypted ? page.Slice(at + 4, 16).ToArray() : default)); at += size;
            }
        }
        if (!StorageFormat.IsZero(page[at..end])) throw StorageFormat.Corrupt("nonzero GiST page padding");
        return new(header, at - 16, offsets.ToArray(), children.ToArray());
    }

    internal static async ValueTask<StoragePageReference> CopyAsync(StoragePager source, StoragePageReference root,
        uint pageCount, StorageImageWriter output, CancellationToken token)
    {
        var seen = new HashSet<uint>();
        async ValueTask<StoragePageReference> Copy(StoragePageReference reference, int depth)
        {
            if (depth > 128 || reference.Page < (source.Encrypted ? 4 : 2) || reference.Page >= pageCount || !seen.Add(reference.Page))
                throw StorageFormat.Corrupt("cyclic, repeated or out-of-bounds GiST page");
            var bytes = await source.ReadPageAsync(reference, token).ConfigureAwait(false);
            var page = Parse(bytes.Span, pageCount, source.Encrypted); var payload = bytes.Slice(16, page.Used).ToArray();
            for (var i = 0; i < page.Children.Length; i++)
            {
                var child = await Copy(page.Children[i], depth + 1).ConfigureAwait(false); var at = page.ChildOffsets[i] - 16;
                BinaryPrimitives.WriteUInt32BigEndian(payload.AsSpan(at), child.Page);
                if (source.Encrypted) child.Tag.Span.CopyTo(payload.AsSpan(at + 4));
            }
            return await output.WriteAsync(output.Reserve(), StorageFormat.EncodePage(source.PageSize, page.Header, payload), token).ConfigureAwait(false);
        }
        return root.Page == 0 ? new(0, source.Encrypted ? new byte[16] : default) : await Copy(root, 0).ConfigureAwait(false);
    }
}
