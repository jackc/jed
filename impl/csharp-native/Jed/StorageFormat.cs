using System.Buffers.Binary;

namespace Jed;

internal readonly record struct StoragePageHeader(byte Type, byte Flags, uint Count, uint Next);
internal readonly record struct StorageManifestEntry(uint Page, ulong Digest);
internal sealed record StorageMeta(int PageSize, ulong TransactionId, uint Root, uint PageCount,
    uint FreeListHead, uint LivePages, uint ManifestHead = 0, uint ManifestEntries = 0,
    uint ManifestPages = 0, ulong ManifestDigest = 0);

internal static class StorageFormat
{
    // Pinned semantic baseline's format version; never follow a supported-core
    // prototype or a later version implicitly.
    internal const ushort Version = 52;
    internal const int HeaderSize = 16;
    internal const int DefaultPageSize = 8192;
    private static readonly uint[] Crc32Table = BuildCrc32Table();
    private static readonly ulong[] Crc64Table = BuildCrc64Table();

    internal static bool ValidPageSize(int size) => size >= 256 && size <= 65536 && (size & (size - 1)) == 0;
    internal static int InlineCapacity(int size) => (size - 64) / 12;
    internal static int OverflowCapacity(int size) => (size - 32) / 12;
    internal static SqlException Corrupt(string message) => new(SqlStates.DataCorrupted, message);

    internal static uint Crc32(ReadOnlySpan<byte> bytes, uint previous = 0)
    {
        var state = ~previous;
        foreach (var value in bytes) state = (state >> 8) ^ Crc32Table[(state ^ value) & 255];
        return ~state;
    }

    internal static ulong Crc64(ReadOnlySpan<byte> bytes, ulong previous = 0)
    {
        var state = previous;
        foreach (var value in bytes) state = (state << 8) ^ Crc64Table[(int)((state >> 56) ^ value)];
        return state;
    }

    internal static uint PageChecksum(ReadOnlySpan<byte> bytes) => Crc32(bytes[16..], Crc32(bytes[..12]));
    internal static uint MetaChecksum(ReadOnlySpan<byte> bytes) => Crc32(bytes[36..], Crc32(bytes[..32]));
    internal static void SealPage(Span<byte> page) => BinaryPrimitives.WriteUInt32BigEndian(page[12..], PageChecksum(page));
    internal static void SealMeta(Span<byte> page) => BinaryPrimitives.WriteUInt32BigEndian(page[32..], MetaChecksum(page));

    internal static StoragePageHeader ParsePage(ReadOnlySpan<byte> page, bool verifyChecksum = true)
    {
        if (!ValidPageSize(page.Length)) throw Corrupt("invalid body page size");
        var type = page[0]; var flags = page[1];
        if (type < 1 || type > 8 || page[2] != 0 || page[3] != 0 ||
            (type == 1 ? (flags & ~1) != 0 : flags != 0)) throw Corrupt("invalid body page header");
        if (verifyChecksum && BinaryPrimitives.ReadUInt32BigEndian(page[12..]) != PageChecksum(page)) throw Corrupt("body page checksum mismatch");
        if (!verifyChecksum && !IsZero(page[12..16])) throw Corrupt("decrypted body page carries a checksum");
        var next = BinaryPrimitives.ReadUInt32BigEndian(page[8..]);
        if ((type is 2 or 3 or 5 or 6) && next != 0) throw Corrupt("tree page has a chain pointer");
        return new(type, flags, BinaryPrimitives.ReadUInt32BigEndian(page[4..]), next);
    }

    internal static byte[] EncodePage(int size, StoragePageHeader header, ReadOnlySpan<byte> payload)
    {
        if (!ValidPageSize(size)) throw Errors.Unsupported("page size");
        if (payload.Length > size - HeaderSize) throw Errors.Unsupported("oversize page payload");
        var page = new byte[size];
        page[0] = header.Type; page[1] = header.Flags;
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(4), header.Count);
        BinaryPrimitives.WriteUInt32BigEndian(page.AsSpan(8), header.Next);
        payload.CopyTo(page.AsSpan(HeaderSize));
        SealPage(page);
        _ = ParsePage(page);
        return page;
    }

    internal static StorageMeta? ParseMeta(ReadOnlySpan<byte> page, long physicalLength)
    {
        if (!ValidPageSize(page.Length) || !page[..4].SequenceEqual("JEDB"u8) ||
            BinaryPrimitives.ReadUInt16BigEndian(page[4..]) != Version || page[6] != 0 || page[7] != 0 ||
            BinaryPrimitives.ReadUInt32BigEndian(page[8..]) != page.Length ||
            BinaryPrimitives.ReadUInt32BigEndian(page[32..]) != MetaChecksum(page) ||
            BinaryPrimitives.ReadUInt32BigEndian(page[60..]) != 0) return null;
        var txid = BinaryPrimitives.ReadUInt64BigEndian(page[12..]);
        var root = BinaryPrimitives.ReadUInt32BigEndian(page[20..]);
        var count = BinaryPrimitives.ReadUInt32BigEndian(page[24..]);
        var free = BinaryPrimitives.ReadUInt32BigEndian(page[28..]);
        var head = BinaryPrimitives.ReadUInt32BigEndian(page[36..]);
        var entries = BinaryPrimitives.ReadUInt32BigEndian(page[40..]);
        var pages = BinaryPrimitives.ReadUInt32BigEndian(page[44..]);
        var digest = BinaryPrimitives.ReadUInt64BigEndian(page[48..]);
        var live = BinaryPrimitives.ReadUInt32BigEndian(page[56..]);
        if (count < 3 || count > physicalLength / page.Length || !BodyPointer(root, count) ||
            (free != 0 && !BodyPointer(free, count)) || live > count - 2 ||
            (ulong)entries + pages > count - 2 ||
            (head == 0) != (pages == 0) || (head != 0 && !BodyPointer(head, count)) ||
            (pages == 0 && digest != 0) || (entries == 0 && pages != 0)) return null;
        var inline = (int)Math.Min(entries, (uint)InlineCapacity(page.Length));
        if (!IsZero(page[(64 + inline * 12)..]) ||
            entries > (ulong)inline + (ulong)pages * (uint)OverflowCapacity(page.Length)) return null;
        return new(page.Length, txid, root, count, free, live, head, entries, pages, digest);
    }

    internal static byte[] EncodeMeta(StorageMeta meta, ReadOnlySpan<StorageManifestEntry> inline = default)
    {
        if (!ValidPageSize(meta.PageSize)) throw Errors.Unsupported("page size");
        if (inline.Length != Math.Min(meta.ManifestEntries, (uint)InlineCapacity(meta.PageSize)))
            throw new ArgumentException("manifest inline count does not match meta");
        var page = new byte[meta.PageSize];
        "JEDB"u8.CopyTo(page);
        BinaryPrimitives.WriteUInt16BigEndian(page.AsSpan(4), Version);
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
        for (var index = 0; index < inline.Length; index++) PutEntry(page.AsSpan(64 + index * 12), inline[index]);
        SealMeta(page);
        if (ParseMeta(page, (long)meta.PageCount * meta.PageSize) is null) throw new ArgumentException("invalid meta fields");
        return page;
    }

    internal static StorageManifestEntry ReadEntry(ReadOnlySpan<byte> bytes) =>
        new(BinaryPrimitives.ReadUInt32BigEndian(bytes), BinaryPrimitives.ReadUInt64BigEndian(bytes[4..]));

    internal static void PutEntry(Span<byte> bytes, StorageManifestEntry entry)
    {
        BinaryPrimitives.WriteUInt32BigEndian(bytes, entry.Page);
        BinaryPrimitives.WriteUInt64BigEndian(bytes[4..], entry.Digest);
    }

    internal static bool BodyPointer(uint id, uint count) => id >= 2 && id < count;
    internal static bool IsZero(ReadOnlySpan<byte> bytes)
    {
        foreach (var value in bytes) if (value != 0) return false;
        return true;
    }

    private static uint[] BuildCrc32Table()
    {
        var result = new uint[256];
        for (uint i = 0; i < result.Length; i++)
        {
            var value = i;
            for (var bit = 0; bit < 8; bit++) value = (value >> 1) ^ ((value & 1) == 0 ? 0 : 0xEDB88320u);
            result[i] = value;
        }
        return result;
    }

    private static ulong[] BuildCrc64Table()
    {
        var result = new ulong[256];
        for (var i = 0; i < result.Length; i++)
        {
            var value = (ulong)i << 56;
            for (var bit = 0; bit < 8; bit++) value = (value << 1) ^ ((value & (1UL << 63)) == 0 ? 0 : 0x42F0E1EBA9EA3693UL);
            result[i] = value;
        }
        return result;
    }
}
