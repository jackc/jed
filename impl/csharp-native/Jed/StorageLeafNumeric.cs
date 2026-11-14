using System.Buffers.Binary;

namespace Jed;

internal sealed partial class StorageLeaf
{
    // Views retain the validated immutable page. Numeric bodies have no invalid
    // bit patterns; page parsing already checked widths, NULL slots and deltas.
    internal readonly struct NumericColumn
    {
        private readonly StorageLeaf leaf;
        private readonly ReadOnlyMemory<byte> bitmap, data;
        private readonly int width, stride, keyOffset;
        private readonly ulong baseValue;

        internal NumericColumn(StorageLeaf leaf, int column)
        {
            this.leaf = leaf;
            var descriptor = leaf.schema.Columns[column];
            width = descriptor.Width; keyOffset = descriptor.KeyOffset;
            if (width is not (2 or 4 or 8)) throw new InvalidOperationException("nonnumeric column width");
            if (keyOffset >= 0) return;
            var region = leaf.Region(column);
            var bitmapLength = (leaf.Count + 7) / 8;
            bitmap = region.Slice(1, bitmapLength);
            var start = 1 + bitmapLength;
            stride = width;
            if (region.Span[0] == 1)
            {
                baseValue = Read(region.Span.Slice(start, width));
                stride = region.Span[start + width];
                start += width + 1;
            }
            data = region[start..];
        }

        internal bool Read(int row, out ulong bits)
        {
            if (keyOffset >= 0)
            {
                Span<byte> key = stackalloc byte[8];
                var destination = key[..width];
                var prefixBytes = Math.Clamp(leaf.prefixLength - keyOffset, 0, width);
                if (prefixBytes != 0) leaf.payload.Span.Slice(leaf.prefixAt + keyOffset, prefixBytes).CopyTo(destination);
                leaf.StoredKey(row).Slice(Math.Max(0, keyOffset - leaf.prefixLength), width - prefixBytes).CopyTo(destination[prefixBytes..]);
                bits = Read(destination); return true;
            }
            if ((bitmap.Span[row / 8] & (0x80 >> (row % 8))) != 0) { bits = 0; return false; }
            bits = baseValue + Read(data.Span.Slice(row * stride, stride));
            return true;
        }

        private static ulong Read(ReadOnlySpan<byte> bytes) => bytes.Length switch
        {
            0 => 0, 1 => bytes[0], 2 => BinaryPrimitives.ReadUInt16BigEndian(bytes),
            4 => BinaryPrimitives.ReadUInt32BigEndian(bytes), 8 => BinaryPrimitives.ReadUInt64BigEndian(bytes),
            _ => throw new InvalidOperationException("numeric column stride")
        };
    }
}
