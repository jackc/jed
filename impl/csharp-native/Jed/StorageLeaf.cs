using System.Buffers.Binary;
using System.Numerics;

namespace Jed;

// Raw storage classes, independent of SQL value ownership. Fixed cells are bare
// value bodies; variable cells are the leaf's tagged bytes; null is SQL NULL.
internal sealed record StorageColumn(int Width = 0, int KeyOffset = -1,
    bool Integer = false, bool Dictionary = false, int ArrayElementWidth = 0);
internal sealed record StorageLeafSchema(StorageColumn[] Columns, int FixedKeyWidth = 0);
internal sealed record StorageRecord(byte[] Key, byte[]?[] Cells)
{
    // Fresh chains have a zero page in their pointer until the commit allocator
    // assigns pages. Stored chains retain only their authenticated pointer.
    internal byte[]?[]? FreshChains { get; init; }
    internal bool Prepared { get; init; }
}

// Keeps directories and column regions in their immutable page buffer. A caller
// may decode one requested cell without materializing all records or columns.
internal sealed class StorageLeaf
{
    private readonly ReadOnlyMemory<byte> payload;
    private readonly StorageLeafSchema schema;
    private readonly int prefixAt, prefixLength, keyDirectory, keyBlob, keyWidth, columnDirectory;
    internal int Count { get; }

    private StorageLeaf(ReadOnlyMemory<byte> page, StorageLeafSchema schema, bool encrypted)
    {
        this.schema = schema;
        var header = StorageFormat.ParsePage(page.Span, verifyChecksum: !encrypted);
        if (header.Type != 2 || header.Count == 0 || header.Count > page.Length - 16)
            throw StorageFormat.Corrupt("invalid leaf header");
        Count = (int)header.Count; payload = page.Slice(16, page.Length - 16 - (encrypted ? StoragePageCodec.Reserve : 0));
        var bytes = payload.Span;
        var flags = bytes[0]; var position = 1;
        if ((flags & ~3) != 0) throw StorageFormat.Corrupt("reserved leaf flags");
        if ((flags & 1) != 0)
        {
            Need(bytes, position, 2); keyWidth = U16(bytes, position); position += 2;
            if (keyWidth == 0) throw StorageFormat.Corrupt("zero fixed key width");
        }
        if ((flags & 2) != 0)
        {
            Need(bytes, position, 2); prefixLength = U16(bytes, position); position += 2; prefixAt = position;
            Need(bytes, position, prefixLength); position += prefixLength;
            if (prefixLength == 0 || (keyWidth != 0 && prefixLength >= keyWidth)) throw StorageFormat.Corrupt("invalid key prefix");
        }
        if (keyWidth != 0)
        {
            keyBlob = position;
            var length = (long)Count * (keyWidth - prefixLength);
            if (length > bytes.Length - position) throw StorageFormat.Corrupt("fixed key blob out of bounds");
            position += (int)length;
        }
        else
        {
            keyDirectory = position;
            Need(bytes, position, Count * 2); position += Count * 2; keyBlob = position;
            var total = ValidateDirectory(bytes.Slice(keyDirectory, Count * 2), Count, bytes.Length - position);
            position += total;
        }
        columnDirectory = position;
        if (schema.Columns.Length > (bytes.Length - position) / 2 - 1) throw StorageFormat.Corrupt("column directory out of bounds");
        var first = position + (schema.Columns.Length + 1) * 2;
        var prior = first;
        for (var c = 0; c <= schema.Columns.Length; c++)
        {
            var offset = U16(bytes, position + c * 2);
            if ((c == 0 && offset != first) || offset < prior || offset > bytes.Length)
                throw StorageFormat.Corrupt("invalid column directory");
            prior = offset;
        }
        if (!StorageFormat.IsZero(bytes[prior..])) throw StorageFormat.Corrupt("nonzero leaf padding");
        ReadOnlySpan<byte> previous = default;
        for (var row = 0; row < Count; row++)
        {
            var key = StoredKey(row);
            if (key.Length + prefixLength == 0 || (row != 0 && previous.SequenceCompareTo(key) >= 0))
                throw StorageFormat.Corrupt("leaf keys not strictly increasing");
            previous = key;
        }
        for (var c = 0; c < schema.Columns.Length; c++) ValidateRegion(c);
    }

    internal static StorageLeaf Parse(ReadOnlyMemory<byte> page, StorageLeafSchema schema, bool encrypted = false) => new(page, schema, encrypted);

    internal byte[] Key(int row)
    {
        ArgumentOutOfRangeException.ThrowIfNegative(row);
        if (row >= Count) throw new ArgumentOutOfRangeException(nameof(row));
        var bytes = payload.Span;
        var start = keyWidth == 0 ? (row == 0 ? 0 : U16(bytes, keyDirectory + (row - 1) * 2)) : row * (keyWidth - prefixLength);
        var end = keyWidth == 0 ? U16(bytes, keyDirectory + row * 2) : start + keyWidth - prefixLength;
        var key = new byte[prefixLength + end - start];
        bytes.Slice(prefixAt, prefixLength).CopyTo(key);
        bytes.Slice(keyBlob + start, end - start).CopyTo(key.AsSpan(prefixLength));
        return key;
    }

    private ReadOnlySpan<byte> StoredKey(int row)
    {
        var bytes = payload.Span;
        var start = keyWidth == 0 ? (row == 0 ? 0 : U16(bytes, keyDirectory + (row - 1) * 2)) : row * (keyWidth - prefixLength);
        var end = keyWidth == 0 ? U16(bytes, keyDirectory + row * 2) : start + keyWidth - prefixLength;
        return bytes.Slice(keyBlob + start, end - start);
    }

    internal ReadOnlyMemory<byte>? Cell(int row, int column)
    {
        if (row < 0 || row >= Count) throw new ArgumentOutOfRangeException(nameof(row));
        var descriptor = schema.Columns[column];
        var region = Region(column); var bytes = region.Span;
        if (descriptor.KeyOffset >= 0) return Key(row).AsMemory(descriptor.KeyOffset, descriptor.Width);
        if (bytes[0] is 0 or 1 && descriptor.Width > 0)
        {
            var bitmap = (Count + 7) / 8;
            if ((bytes[1 + row / 8] & (0x80 >> (row % 8))) != 0) return null;
            if (bytes[0] == 0) return region.Slice(1 + bitmap + row * descriptor.Width, descriptor.Width);
            var value = ReadUnsigned(bytes.Slice(1 + bitmap, descriptor.Width));
            var offsetWidth = bytes[1 + bitmap + descriptor.Width];
            value += ReadUnsigned(bytes.Slice(2 + bitmap + descriptor.Width + row * offsetWidth, offsetWidth));
            return WriteUnsigned(value, descriptor.Width);
        }
        if (bytes[0] == 0)
        {
            var start = row == 0 ? 0 : U16(bytes, 1 + (row - 1) * 2);
            var end = U16(bytes, 1 + row * 2);
            if (start == end) return null;
            return region.Slice(1 + Count * 2 + start, end - start);
        }
        if (bytes[0] == 2)
        {
            var count = U16(bytes, 1);
            var blob = 3 + count * 2;
            var codes = blob + U16(bytes, 3 + (count - 1) * 2);
            var code = bytes[codes + row];
            var start = code == 0 ? 0 : U16(bytes, 3 + (code - 1) * 2);
            var end = U16(bytes, 3 + code * 2);
            if (start == end) return null;
            return region.Slice(blob + start, end - start);
        }
        // Packed arrays rebuild only the requested array's ordinary inline body.
        var n = bytes[1 + row];
        if (n == 255) return null;
        if (n == 0) return new byte[] { 0, 0, 0 };
        var width = descriptor.ArrayElementWidth;
        var baseValue = ReadUnsigned(bytes.Slice(1 + Count, width));
        var bits = bytes[1 + Count + width];
        var bitStart = 0;
        for (var i = 0; i < row; i++) if (bytes[1 + i] != 255) bitStart += bytes[1 + i] * bits;
        var result = new byte[11 + n * width]; result[1] = 1;
        BinaryPrimitives.WriteUInt32BigEndian(result.AsSpan(3), n);
        BinaryPrimitives.WriteUInt32BigEndian(result.AsSpan(7), 1);
        var packed = bytes[(2 + Count + width)..];
        for (var i = 0; i < n; i++) WriteUnsigned(baseValue + ReadBits(packed, bitStart + i * bits, bits), width).CopyTo(result, 11 + i * width);
        return result;
    }

    internal StorageRecord Record(int row)
    {
        var cells = new byte[]?[schema.Columns.Length];
        for (var c = 0; c < cells.Length; c++) cells[c] = Cell(row, c)?.ToArray();
        return new(Key(row), cells);
    }

    private ReadOnlyMemory<byte> Region(int column)
    {
        var start = U16(payload.Span, columnDirectory + column * 2);
        var end = U16(payload.Span, columnDirectory + (column + 1) * 2);
        return payload.Slice(start, end - start);
    }

    private void ValidateRegion(int column)
    {
        var descriptor = schema.Columns[column];
        var bytes = Region(column).Span;
        if (descriptor.KeyOffset >= 0)
        {
            if (!bytes.IsEmpty || descriptor.Width <= 0) throw StorageFormat.Corrupt("stored elided key column");
            for (var row = 0; row < Count; row++)
                if (StoredKey(row).Length + prefixLength < (long)descriptor.KeyOffset + descriptor.Width) throw StorageFormat.Corrupt("elided key column out of bounds");
            return;
        }
        if (bytes.IsEmpty) throw StorageFormat.Corrupt("missing column region");
        var kind = bytes[0];
        if (descriptor.Width > 0 && kind is 0 or 1)
        {
            var bitmap = (Count + 7) / 8;
            Need(bytes, 1, bitmap);
            if (Count % 8 != 0 && (bytes[bitmap] & ((1 << (8 - Count % 8)) - 1)) != 0) throw StorageFormat.Corrupt("nonzero bitmap padding");
            var stride = descriptor.Width; var start = 1 + bitmap; ulong baseValue = 0;
            if (kind == 1)
            {
                if (!descriptor.Integer || descriptor.Width > 8) throw StorageFormat.Corrupt("offset encoding on noninteger column");
                Need(bytes, start, descriptor.Width + 1);
                baseValue = ReadUnsigned(bytes.Slice(start, descriptor.Width)); stride = bytes[start + descriptor.Width];
                start += descriptor.Width + 1;
                if (stride is not (0 or 1 or 2 or 4) || stride >= descriptor.Width) throw StorageFormat.Corrupt("invalid integer offset width");
            }
            if ((long)start + (long)stride * Count != bytes.Length) throw StorageFormat.Corrupt("invalid fixed region size");
            for (var row = 0; row < Count; row++)
            {
                var cell = bytes.Slice(start + row * stride, stride);
                if ((bytes[1 + row / 8] & (0x80 >> (row % 8))) != 0)
                {
                    if (!StorageFormat.IsZero(cell)) throw StorageFormat.Corrupt("nonzero NULL slot");
                }
                else if (kind == 1 && ReadUnsigned(cell) > Maximum(descriptor.Width) - baseValue)
                    throw StorageFormat.Corrupt("integer offset overflow");
            }
            return;
        }
        if (kind == 0 && descriptor.Width == 0)
        {
            Need(bytes, 1, Count * 2);
            var total = ValidateDirectory(bytes.Slice(1, Count * 2), Count, bytes.Length - 1 - Count * 2);
            if (total != bytes.Length - 1 - Count * 2) throw StorageFormat.Corrupt("variable region trailing bytes");
            for (var row = 0; row < Count; row++) ValidateCell(Cell(row, column), descriptor);
            return;
        }
        if (kind == 2 && descriptor.Dictionary && descriptor.Width == 0)
        {
            Need(bytes, 1, 2); var count = U16(bytes, 1);
            if (count is < 1 or > 256) throw StorageFormat.Corrupt("invalid dictionary count");
            Need(bytes, 3, count * 2);
            var total = ValidateDirectory(bytes.Slice(3, count * 2), count, bytes.Length - 3 - count * 2);
            var codes = 3 + count * 2 + total;
            if (bytes.Length - codes != Count) throw StorageFormat.Corrupt("invalid dictionary codes size");
            for (var row = 0; row < Count; row++)
            {
                if (bytes[codes + row] >= count) throw StorageFormat.Corrupt("dictionary code out of range");
                ValidateCell(Cell(row, column), descriptor);
            }
            return;
        }
        if (kind == 4 && descriptor.Width == 0 && descriptor.ArrayElementWidth is 2 or 4 or 8)
        {
            var width = descriptor.ArrayElementWidth;
            Need(bytes, 1, Count + width + 1);
            var baseValue = ReadUnsigned(bytes.Slice(1 + Count, width)); var bits = bytes[1 + Count + width];
            if (bits > width * 8) throw StorageFormat.Corrupt("invalid packed array bit width");
            var elements = 0;
            for (var row = 0; row < Count; row++) if (bytes[1 + row] != 255) elements += bytes[1 + row];
            var bitCount = (long)elements * bits;
            if (bytes.Length != 2 + Count + width + (bitCount + 7) / 8) throw StorageFormat.Corrupt("packed array size mismatch");
            var packed = bytes[(2 + Count + width)..];
            if (bitCount % 8 != 0 && (packed[^1] & ((1 << (8 - (int)(bitCount % 8))) - 1)) != 0)
                throw StorageFormat.Corrupt("packed array nonzero padding");
            for (var i = 0; i < elements; i++)
                if (ReadBits(packed, i * bits, bits) > Maximum(width) - baseValue) throw StorageFormat.Corrupt("packed array overflow");
            return;
        }
        throw StorageFormat.Corrupt("column region kind does not match schema");
    }

    private static void ValidateCell(ReadOnlyMemory<byte>? cell, StorageColumn descriptor)
    {
        if (cell is not { } value) return;
        if (value.IsEmpty) throw StorageFormat.Corrupt("empty present leaf cell");
        var tag = value.Span[0];
        if (tag is 1 or > 5 || (tag == 5) != (descriptor.Dictionary && tag is not (2 or 3 or 4)))
            throw StorageFormat.Corrupt("invalid leaf cell tag");
    }

    internal static byte[] Encode(int pageSize, StorageLeafSchema schema, IReadOnlyList<StorageRecord> rows, bool encrypted = false)
    {
        var payload = EncodePayload(schema, rows);
        if (payload.Length > pageSize - 16 - (encrypted ? StoragePageCodec.Reserve : 0)) throw Errors.Unsupported("oversize leaf page");
        return StorageFormat.EncodePage(pageSize, new(2, 0, (uint)rows.Count, 0), payload);
    }

    internal static long MeasurePayload(StorageLeafSchema schema, IReadOnlyList<StorageRecord> rows)
    {
        if (rows.Count == 0) return 0;
        var prefix = SharedPrefix(rows);
        var size = 1L + (schema.FixedKeyWidth == 0 ? 2L * rows.Count : 2) +
            (prefix == 0 ? 0 : 2 + prefix) + rows.Sum(row => (long)row.Key.Length - prefix) +
            2L * (schema.Columns.Length + 1);
        for (var c = 0; c < schema.Columns.Length; c++) size += EncodeRegion(schema.Columns[c], rows, c).Length;
        return size;
    }

    internal static byte[] EncodePayload(StorageLeafSchema schema, IReadOnlyList<StorageRecord> rows)
    {
        if (rows.Count == 0) throw new ArgumentException("empty tree has no leaf");
        var prefix = SharedPrefix(rows);
        using var stream = new MemoryStream();
        stream.WriteByte((byte)((schema.FixedKeyWidth != 0 ? 1 : 0) | (prefix != 0 ? 2 : 0)));
        if (schema.FixedKeyWidth != 0) PutU16(stream, schema.FixedKeyWidth);
        if (prefix != 0) { PutU16(stream, prefix); stream.Write(rows[0].Key.AsSpan(0, prefix)); }
        var total = 0;
        for (var row = 0; row < rows.Count; row++)
        {
            var record = rows[row];
            if (record.Key.Length == 0 || record.Cells.Length != schema.Columns.Length ||
                (schema.FixedKeyWidth != 0 && record.Key.Length != schema.FixedKeyWidth) ||
                (row != 0 && rows[row - 1].Key.AsSpan().SequenceCompareTo(record.Key) >= 0))
                throw new ArgumentException("invalid leaf records");
            total = checked(total + record.Key.Length - prefix);
            if (schema.FixedKeyWidth == 0) PutU16(stream, total);
        }
        foreach (var row in rows) stream.Write(row.Key.AsSpan(prefix));
        var regions = new byte[schema.Columns.Length][];
        for (var c = 0; c < regions.Length; c++) regions[c] = EncodeRegion(schema.Columns[c], rows, c);
        var position = checked((int)stream.Position + (regions.Length + 1) * 2);
        PutU16(stream, position);
        foreach (var region in regions) { position = checked(position + region.Length); PutU16(stream, position); }
        foreach (var region in regions) stream.Write(region);
        return stream.ToArray();
    }

    private static byte[] EncodeRegion(StorageColumn descriptor, IReadOnlyList<StorageRecord> rows, int column)
    {
        if (descriptor.KeyOffset >= 0)
        {
            foreach (var row in rows)
                if (row.Cells[column] is not { } value || value.Length != descriptor.Width ||
                    row.Key.Length < (long)descriptor.KeyOffset + descriptor.Width ||
                    !row.Key.AsSpan(descriptor.KeyOffset, descriptor.Width).SequenceEqual(value))
                    throw new ArgumentException("key-elided column differs from key");
            return [];
        }
        using var plain = new MemoryStream(); plain.WriteByte(0);
        if (descriptor.Width > 0)
        {
            var bitmap = new byte[(rows.Count + 7) / 8];
            for (var i = 0; i < rows.Count; i++) if (rows[i].Cells[column] is null) bitmap[i / 8] |= (byte)(0x80 >> (i % 8));
            plain.Write(bitmap);
            var minimum = ulong.MaxValue; ulong maximum = 0; var any = false;
            foreach (var row in rows)
            {
                var value = row.Cells[column];
                if (value is null) plain.Write(new byte[descriptor.Width]);
                else
                {
                    if (value.Length != descriptor.Width) throw new ArgumentException("wrong fixed cell width");
                    plain.Write(value);
                    if (descriptor.Integer)
                    {
                        var number = ReadUnsigned(value); minimum = Math.Min(minimum, number); maximum = Math.Max(maximum, number); any = true;
                    }
                }
            }
            if (!descriptor.Integer) return plain.ToArray();
            if (!any) minimum = maximum = 0;
            var range = maximum - minimum;
            var width = range == 0 ? 0 : range <= byte.MaxValue ? 1 : range <= ushort.MaxValue ? 2 : range <= uint.MaxValue ? 4 : 8;
            if (width >= descriptor.Width || (long)rows.Count * (descriptor.Width - width) <= descriptor.Width + 1) return plain.ToArray();
            using var compact = new MemoryStream(); compact.WriteByte(1); compact.Write(bitmap);
            compact.Write(WriteUnsigned(minimum, descriptor.Width)); compact.WriteByte((byte)width);
            foreach (var row in rows) compact.Write(WriteUnsigned(row.Cells[column] is { } value ? ReadUnsigned(value) - minimum : 0, width));
            return compact.ToArray();
        }
        var total = 0;
        foreach (var row in rows)
        {
            var cell = row.Cells[column];
            if (cell is not null && cell.Length == 0) throw new ArgumentException("present variable cell cannot be empty");
            if (cell is not null) ValidateCell(cell.AsMemory(), descriptor);
            total = checked(total + (cell?.Length ?? 0));
        }
        byte[] result;
        if (total <= ushort.MaxValue)
        {
            var end = 0;
            foreach (var row in rows) { end += row.Cells[column]?.Length ?? 0; PutU16(plain, end); }
            foreach (var row in rows) if (row.Cells[column] is { } value) plain.Write(value);
            result = plain.ToArray();
        }
        else
        {
            // Size-only candidate: no directory can represent this region. A
            // dictionary/packed candidate may still fit. If none does, the leaf
            // encoder rejects the payload before copying any such placeholder;
            // the split planner still needs its exact (unbounded) measured size.
            result = new byte[checked(1 + rows.Count * 2 + total)];
        }
        if (descriptor.Dictionary)
        {
            var entries = new List<byte[]?>(); var codes = new byte[rows.Count];
            for (var i = 0; i < rows.Count; i++)
            {
                var value = rows[i].Cells[column]; var found = -1;
                if (value is null || value[0] == 5)
                    for (var j = 0; j < entries.Count; j++)
                        if (value is null ? entries[j] is null : entries[j] is { } prior && value.AsSpan().SequenceEqual(prior)) { found = j; break; }
                if (found < 0) { found = entries.Count; entries.Add(value); }
                if (found > 255) return result;
                codes[i] = (byte)found;
            }
            total = entries.Sum(value => value?.Length ?? 0);
            var compactSize = checked(3 + entries.Count * 2 + total + codes.Length);
            if (compactSize < result.Length)
            {
                if (total > ushort.MaxValue) result = new byte[compactSize];
                else
                {
                    using var compact = new MemoryStream(); compact.WriteByte(2); PutU16(compact, entries.Count); var end = 0;
                    foreach (var value in entries) { end += value?.Length ?? 0; PutU16(compact, end); }
                    foreach (var value in entries) if (value is not null) compact.Write(value);
                    compact.Write(codes); result = compact.ToArray();
                }
            }
        }
        if (descriptor.ArrayElementWidth is 2 or 4 or 8)
        {
            var packed = EncodeArrays(rows, column, descriptor.ArrayElementWidth);
            if (packed is not null && packed.Length < result.Length) result = packed;
        }
        return result;
    }

    private static byte[]? EncodeArrays(IReadOnlyList<StorageRecord> rows, int column, int width)
    {
        var counts = new byte[rows.Count]; var elements = new List<ulong>();
        ulong minimum = ulong.MaxValue, maximum = 0;
        for (var row = 0; row < rows.Count; row++)
        {
            var value = rows[row].Cells[column];
            if (value is null) { counts[row] = 255; continue; }
            if (value.Length == 3 && StorageFormat.IsZero(value)) continue;
            if (value.Length < 11 || value[0] != 0 || value[1] != 1 || value[2] != 0 ||
                BinaryPrimitives.ReadUInt32BigEndian(value.AsSpan(7)) != 1) return null;
            var count = BinaryPrimitives.ReadUInt32BigEndian(value.AsSpan(3));
            if (count is < 1 or > 254 || value.Length != 11 + count * width) return null;
            counts[row] = (byte)count;
            for (var i = 0; i < count; i++)
            {
                var element = ReadUnsigned(value.AsSpan(11 + i * width, width));
                elements.Add(element); minimum = Math.Min(minimum, element); maximum = Math.Max(maximum, element);
            }
        }
        if (elements.Count == 0) minimum = maximum = 0;
        var bits = 64 - BitOperations.LeadingZeroCount(maximum - minimum);
        var packed = new byte[(checked(elements.Count * bits) + 7) / 8]; var at = 0;
        foreach (var value in elements)
        {
            var offset = value - minimum;
            for (var bit = bits - 1; bit >= 0; bit--, at++)
                if ((offset & (1UL << bit)) != 0) packed[at / 8] |= (byte)(0x80 >> (at % 8));
        }
        using var stream = new MemoryStream(); stream.WriteByte(4); stream.Write(counts);
        stream.Write(WriteUnsigned(minimum, width)); stream.WriteByte((byte)bits); stream.Write(packed);
        return stream.ToArray();
    }

    private static int ValidateDirectory(ReadOnlySpan<byte> directory, int count, int available)
    {
        var prior = 0;
        for (var i = 0; i < count; i++)
        {
            var next = U16(directory, i * 2);
            if (next < prior || next > available) throw StorageFormat.Corrupt("invalid end-offset directory");
            prior = next;
        }
        return prior;
    }
    private static int SharedPrefix(IReadOnlyList<StorageRecord> rows)
    {
        if (rows.Count < 2) return 0;
        var first = rows[0].Key; var last = rows[^1].Key; var prefix = 0;
        while (prefix < Math.Min(65535, Math.Min(first.Length, last.Length)) && first[prefix] == last[prefix]) prefix++;
        return (long)rows.Count * prefix > prefix + 2 ? prefix : 0;
    }
    private static int U16(ReadOnlySpan<byte> bytes, int position) => BinaryPrimitives.ReadUInt16BigEndian(bytes[position..]);
    private static void Need(ReadOnlySpan<byte> bytes, int position, int count)
    {
        if (position < 0 || count < 0 || count > bytes.Length - position) throw StorageFormat.Corrupt("truncated leaf region");
    }
    private static void PutU16(Stream stream, int value)
    {
        if ((uint)value > ushort.MaxValue) throw Errors.Unsupported("oversize leaf payload");
        Span<byte> bytes = stackalloc byte[2]; BinaryPrimitives.WriteUInt16BigEndian(bytes, (ushort)value); stream.Write(bytes);
    }
    private static ulong Maximum(int width) => width == 8 ? ulong.MaxValue : (1UL << (width * 8)) - 1;
    private static ulong ReadUnsigned(ReadOnlySpan<byte> bytes)
    {
        ulong result = 0;
        foreach (var value in bytes) result = (result << 8) | value;
        return result;
    }
    private static byte[] WriteUnsigned(ulong value, int width)
    {
        var result = new byte[width];
        for (var i = width - 1; i >= 0; i--) { result[i] = (byte)(value & 255); value >>= 8; }
        return result;
    }
    private static ulong ReadBits(ReadOnlySpan<byte> bytes, int start, int count)
    {
        ulong result = 0;
        for (var i = start; i < start + count; i++) result = (result << 1) | (uint)((bytes[i / 8] >> (7 - i % 8)) & 1);
        return result;
    }
}
