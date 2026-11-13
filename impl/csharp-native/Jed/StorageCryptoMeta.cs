using System.Buffers.Binary;

namespace Jed;

internal sealed record StorageEncryptedMeta(StorageMeta Header, byte[] RootTag, byte[] FreeListTag);
internal static class StorageCryptoMeta
{
    internal static byte[] Encode(StorageMeta meta, ReadOnlySpan<byte> rootTag, ReadOnlySpan<byte> freeTag, ReadOnlySpan<StorageManifestEntry> entries)
    {
        if (meta.PageSize < 1024 || !StorageFormat.ValidPageSize(meta.PageSize) || rootTag.Length != 16 || freeTag.Length != 16 ||
            entries.Length != Math.Min(meta.ManifestEntries, (uint)InlineCapacity(meta.PageSize))) throw new ArgumentException("invalid encrypted meta fields");
        var page = new byte[meta.PageSize]; "JEDB"u8.CopyTo(page);
        BinaryPrimitives.WriteUInt16BigEndian(page.AsSpan(4), StorageFormat.Version); page[6] = 1; page[7] = 40;
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(8), (uint)meta.PageSize);
        BinaryPrimitives.WriteUInt64BigEndian(page.AsSpan(12), meta.TransactionId);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(20), meta.Root);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(24), meta.PageCount);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(28), meta.FreeListHead);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(36), meta.ManifestHead);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(40), meta.ManifestEntries);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(44), meta.ManifestPages);
        BinaryPrimitives.WriteUInt64BigEndian(page.AsSpan(48), meta.ManifestDigest);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(56), meta.LivePages);
        rootTag.CopyTo(page.AsSpan(64)); freeTag.CopyTo(page.AsSpan(80));
        for (var i = 0; i < entries.Length; i++) StorageFormat.PutEntry(page.AsSpan(96 + i * 12), entries[i]);
        StorageFormat.SealMeta(page);
        if (Parse(page, (long)meta.PageCount * meta.PageSize) is null) throw new ArgumentException("invalid encrypted meta fields");
        return page;
    }
    internal static int InlineCapacity(int size) => (size - 96 - StoragePageCodec.Reserve) / 12;
    // Keyless candidate validation. Authentication is a distinct step AFTER
    // complete-manifest selection, and an authentication failure never falls back.
    internal static StorageEncryptedMeta? Parse(ReadOnlySpan<byte> page, long physicalLength)
    {
        if (!StorageFormat.ValidPageSize(page.Length) || page.Length < 1024 || !page[..4].SequenceEqual("JEDB"u8) ||
            BinaryPrimitives.ReadUInt16BigEndian(page[4..]) != StorageFormat.Version || page[6] != 1 || page[7] != StoragePageCodec.Reserve ||
            BinaryPrimitives.ReadUInt32BigEndian(page[8..]) != page.Length ||
            BinaryPrimitives.ReadUInt32BigEndian(page[32..]) != StorageFormat.MetaChecksum(page) || !StorageFormat.IsZero(page[60..64])) return null;
        var txid = BinaryPrimitives.ReadUInt64BigEndian(page[12..]); var root = BinaryPrimitives.ReadUInt32BigEndian(page[20..]);
        var count = BinaryPrimitives.ReadUInt32BigEndian(page[24..]); var free = BinaryPrimitives.ReadUInt32BigEndian(page[28..]);
        var head = BinaryPrimitives.ReadUInt32BigEndian(page[36..]); var entries = BinaryPrimitives.ReadUInt32BigEndian(page[40..]);
        var pages = BinaryPrimitives.ReadUInt32BigEndian(page[44..]); var digest = BinaryPrimitives.ReadUInt64BigEndian(page[48..]);
        var live = BinaryPrimitives.ReadUInt32BigEndian(page[56..]);
        bool Pointer(uint id) => id >= 4 && id < count;
        if (count < 5 || count > physicalLength / page.Length || !Pointer(root) || (free != 0 && !Pointer(free)) || live > count - 4 ||
            (ulong)entries + pages > count - 4 || (head == 0) != (pages == 0) || (head != 0 && !Pointer(head)) ||
            (pages == 0 && digest != 0) || (entries == 0 && pages != 0) || (free == 0 && !StorageFormat.IsZero(page[80..96]))) return null;
        var inline = (int)Math.Min(entries, (uint)InlineCapacity(page.Length));
        if (!StorageFormat.IsZero(page[(96 + inline * 12)..^StoragePageCodec.Reserve]) ||
            entries > (ulong)inline + (ulong)pages * (uint)StorageFormat.OverflowCapacity(page.Length)) return null;
        return new(new(page.Length, txid, root, count, free, live, head, entries, pages, digest), page[64..80].ToArray(), page[80..96].ToArray());
    }
}
