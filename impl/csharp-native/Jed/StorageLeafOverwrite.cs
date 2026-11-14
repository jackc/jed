using System.Buffers.Binary;

namespace Jed;

internal sealed partial class StorageLeaf
{
    internal ReadOnlyMemory<byte> EncodedPage => encodedPage;
    internal int PayloadLength => U16(payload.Span, columnDirectory + schema.Columns.Length * 2);

    // Keep the leaf packed when an overwrite changes only fixed-width cells.
    // The untouched variable regions (including external pointers) are copied
    // verbatim; integer regions are recoded by the same canonical minimum/range
    // rule as EncodeRegion. Each result owns fresh bytes, so an older tree and
    // every cursor over it remain immutable.
    internal StorageLeaf? ReplaceFixed(int row, StorageRecord replacement, bool encrypted)
    {
        if (replacement.FreshChains?.Any(chain => chain is not null) == true || CompareKey(row, replacement.Key) != 0) return null;
        var regions = new byte[]?[schema.Columns.Length];
        var length = PayloadLength;
        for (var column = 0; column < schema.Columns.Length; column++)
        {
            var descriptor = schema.Columns[column];
            var value = replacement.Cells[column];
            if (descriptor.KeyOffset >= 0) continue;
            var existing = Cell(row, column);
            if (value is null ? existing is null : existing is { } prior && value.AsSpan().SequenceEqual(prior.Span)) continue;
            if (descriptor.Width == 0) return null;
            var region = EncodeFixedReplacement(row, column, value);
            regions[column] = region;
            length = checked(length - Region(column).Length + region.Length);
        }
        if (length > payload.Length) return null;
        var encoded = new byte[encodedPage.Length];
        encodedPage.Span[..16].CopyTo(encoded);
        payload.Span[..columnDirectory].CopyTo(encoded.AsSpan(16));
        var position = columnDirectory + (schema.Columns.Length + 1) * 2;
        BinaryPrimitives.WriteUInt16BigEndian(encoded.AsSpan(16 + columnDirectory), checked((ushort)position));
        for (var column = 0; column < schema.Columns.Length; column++)
        {
            var region = regions[column] is { } changed ? changed.AsSpan() : Region(column).Span;
            region.CopyTo(encoded.AsSpan(16 + position)); position += region.Length;
            BinaryPrimitives.WriteUInt16BigEndian(encoded.AsSpan(16 + columnDirectory + (column + 1) * 2), checked((ushort)position));
        }
        StorageFormat.SealPage(encoded);
        return new(encoded, schema, encrypted, verifyChecksum: true);
    }

    private byte[] EncodeFixedReplacement(int edited, int column, byte[]? replacement)
    {
        var descriptor = schema.Columns[column]; var width = descriptor.Width;
        if (replacement is not null && replacement.Length != width) throw new ArgumentException("wrong fixed cell width");
        var source = Region(column).Span;
        var bitmap = (Count + 7) / 8;
        var stride = source[0] == 0 ? width : source[1 + bitmap + width];
        var start = source[0] == 0 ? 1 + bitmap : 2 + bitmap + width;
        var baseValue = source[0] == 0 ? 0 : ReadUnsigned(source.Slice(1 + bitmap, width));
        ulong minimum = ulong.MaxValue, maximum = 0; var any = false;
        if (descriptor.Integer)
        {
            for (var i = 0; i < Count; i++)
            {
                if (i == edited ? replacement is null : (source[1 + i / 8] & (0x80 >> (i % 8))) != 0) continue;
                var value = i == edited ? ReadUnsigned(replacement!) : baseValue + ReadUnsigned(source.Slice(start + i * stride, stride));
                minimum = Math.Min(minimum, value); maximum = Math.Max(maximum, value); any = true;
            }
        }
        if (!any) minimum = maximum = 0;
        var range = maximum - minimum;
        var compactWidth = range == 0 ? 0 : range <= byte.MaxValue ? 1 : range <= ushort.MaxValue ? 2 : range <= uint.MaxValue ? 4 : 8;
        var compact = descriptor.Integer && compactWidth < width && (long)Count * (width - compactWidth) > width + 1;
        var targetStride = compact ? compactWidth : width;
        var targetStart = 1 + bitmap + (compact ? width + 1 : 0);
        var result = new byte[targetStart + Count * targetStride]; result[0] = compact ? (byte)1 : (byte)0;
        source.Slice(1, bitmap).CopyTo(result.AsSpan(1));
        var mask = (byte)(0x80 >> (edited % 8));
        if (replacement is null) result[1 + edited / 8] |= mask;
        else result[1 + edited / 8] &= unchecked((byte)~mask);
        if (compact)
        {
            PutUnsigned(result.AsSpan(1 + bitmap, width), minimum);
            result[1 + bitmap + width] = (byte)compactWidth;
        }
        for (var i = 0; i < Count; i++)
        {
            if ((result[1 + i / 8] & (0x80 >> (i % 8))) != 0) continue;
            var target = result.AsSpan(targetStart + i * targetStride, targetStride);
            if (!descriptor.Integer)
            {
                if (i == edited) replacement!.AsSpan().CopyTo(target);
                else source.Slice(start + i * stride, stride).CopyTo(target);
            }
            else
            {
                var value = i == edited ? ReadUnsigned(replacement!) : baseValue + ReadUnsigned(source.Slice(start + i * stride, stride));
                PutUnsigned(target, compact ? value - minimum : value);
            }
        }
        return result;
    }

    private static void PutUnsigned(Span<byte> destination, ulong value)
    {
        for (var i = destination.Length - 1; i >= 0; i--) { destination[i] = unchecked((byte)value); value >>= 8; }
    }
}
