using System.Buffers.Binary;

namespace Jed;

internal readonly record struct StorageRemapEntry(uint Parent, ushort Slot, uint Page, ReadOnlyMemory<byte> Tag);

// Generation-owned, parent/slot-scoped substitutions. Never attach these to the
// shared page cache: concurrent readers can resolve identical stored bytes under
// different committed generations.
internal static class StorageRemap
{
    internal static int Capacity(int size, bool encrypted) => ((size - (encrypted ? 136 : 64)) / 3) / (encrypted ? 26 : 10);
    internal static int Start(int size, bool encrypted) => size - (encrypted ? 40 : 0) - Capacity(size, encrypted) * (encrypted ? 26 : 10);
    internal static int InlineCapacity(int size, bool encrypted) => (Start(size, encrypted) - (encrypted ? 96 : 64)) / 8;
    internal static StorageRemapEntry[]? Parse(ReadOnlySpan<byte> bytes, uint pageCount, bool encrypted)
    {
        var count = BinaryPrimitives.ReadUInt32BigEndian(bytes[60..]);
        if (count > Capacity(bytes.Length, encrypted)) return null;
        StorageRemapEntry[] result = count == 0 ? [] : new StorageRemapEntry[count]; var offset = Start(bytes.Length, encrypted);
        var targets = new HashSet<uint>(); var minimum = encrypted ? 4u : 2u;
        for (var i = 0; i < result.Length; i++)
        {
            var parent = BinaryPrimitives.ReadUInt32BigEndian(bytes[offset..]);
            var slot = BinaryPrimitives.ReadUInt16BigEndian(bytes[(offset + 4)..]);
            var page = BinaryPrimitives.ReadUInt32BigEndian(bytes[(offset + 6)..]);
            if (parent < minimum || parent >= pageCount || page < minimum || page >= pageCount || page == parent || !targets.Add(page)) return null;
            if (i > 0 && (parent < result[i - 1].Parent || parent == result[i - 1].Parent && slot <= result[i - 1].Slot)) return null;
            result[i] = new(parent, slot, page, encrypted ? bytes.Slice(offset + 10, 16).ToArray() : default);
            offset += encrypted ? 26 : 10;
        }
        return StorageFormat.IsZero(bytes[offset..(bytes.Length - (encrypted ? 40 : 0))]) ? result : null;
    }
    internal static void Encode(Span<byte> bytes, StorageRemapEntry[] entries, bool encrypted)
    {
        if (entries.Length > Capacity(bytes.Length, encrypted)) throw new ArgumentException("remap capacity exceeded");
        BinaryPrimitives.WriteUInt32BigEndian(bytes[60..], (uint)entries.Length);
        var offset = Start(bytes.Length, encrypted);
        foreach (var entry in entries)
        {
            BinaryPrimitives.WriteUInt32BigEndian(bytes[offset..], entry.Parent);
            BinaryPrimitives.WriteUInt16BigEndian(bytes[(offset + 4)..], entry.Slot);
            BinaryPrimitives.WriteUInt32BigEndian(bytes[(offset + 6)..], entry.Page);
            if (encrypted) entry.Tag.Span.CopyTo(bytes[(offset + 10)..]);
            offset += encrypted ? 26 : 10;
        }
    }
}
