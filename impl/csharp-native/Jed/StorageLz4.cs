using System.Buffers;
using System.Buffers.Binary;

namespace Jed;

// The language-neutral pinned block algorithm, not a runtime library's choice
// of match search. Hash insertion, greedy extension, and tail rules are bytes.
internal static class StorageLz4
{
    internal static byte[] Compress(ReadOnlySpan<byte> source)
    {
        var output = new ArrayBufferWriter<byte>(); var table = new int[4096]; Array.Fill(table, -1);
        var anchor = 0; var position = 0; var limit = source.Length - 12;
        while (position <= limit)
        {
            var word = BinaryPrimitives.ReadUInt32LittleEndian(source[position..]);
            var hash = unchecked(word * 2654435761u) >> 20;
            var candidate = table[hash]; table[hash] = position;
            if (candidate >= 0 && position - candidate <= ushort.MaxValue && BinaryPrimitives.ReadUInt32LittleEndian(source[candidate..]) == word)
            {
                var length = 4;
                while (length < source.Length - 5 - position && source[candidate + length] == source[position + length]) length++;
                Sequence(output, source[anchor..position], position - candidate, length);
                position += length; anchor = position;
            }
            else position++;
        }
        Sequence(output, source[anchor..], 0, 0);
        return output.WrittenSpan.ToArray();
    }
    private static void Sequence(ArrayBufferWriter<byte> output, ReadOnlySpan<byte> literals, int offset, int match)
    {
        Byte(output, (byte)((Math.Min(literals.Length, 15) << 4) | (match == 0 ? 0 : Math.Min(match - 4, 15))));
        if (literals.Length >= 15) Extension(output, literals.Length - 15);
        output.Write(literals);
        if (match == 0) return;
        var span = output.GetSpan(2); BinaryPrimitives.WriteUInt16LittleEndian(span, (ushort)offset); output.Advance(2);
        if (match - 4 >= 15) Extension(output, match - 19);
    }
    private static void Extension(ArrayBufferWriter<byte> output, int remaining)
    {
        while (remaining >= 255) { Byte(output, 255); remaining -= 255; }
        Byte(output, (byte)remaining);
    }
    private static void Byte(ArrayBufferWriter<byte> output, byte value) { output.GetSpan(1)[0] = value; output.Advance(1); }

    internal static byte[] Decompress(ReadOnlySpan<byte> source, int expectedLength)
    {
        if (expectedLength < 0) throw StorageFormat.Corrupt("negative decompressed value length");
        // Validate before allocating the declared output: hostile length fields
        // cannot make a malformed tiny block allocate a huge destination.
        Decode(source, expectedLength, default, write: false);
        var result = new byte[expectedLength]; Decode(source, expectedLength, result, write: true); return result;
    }
    private static void Decode(ReadOnlySpan<byte> source, int expected, Span<byte> destination, bool write)
    {
        var position = 0; var produced = 0;
        while (position < source.Length)
        {
            var token = source[position++];
            var literals = Length(source, ref position, token >> 4, expected - produced);
            if (literals > source.Length - position) throw StorageFormat.Corrupt("truncated compressed literals");
            if (write) source.Slice(position, literals).CopyTo(destination[produced..]);
            position += literals; produced += literals;
            if (position == source.Length)
            {
                if (produced != expected) throw StorageFormat.Corrupt("decompressed value length differs");
                return;
            }
            if (source.Length - position < 2) throw StorageFormat.Corrupt("truncated compressed offset");
            var offset = BinaryPrimitives.ReadUInt16LittleEndian(source[position..]); position += 2;
            if (offset == 0 || offset > produced) throw StorageFormat.Corrupt("invalid compressed back-reference");
            if (expected - produced < 4) throw StorageFormat.Corrupt("compressed match exceeds declared length");
            var match = 4 + Length(source, ref position, token & 15, expected - produced - 4);
            if (write) for (var i = 0; i < match; i++) destination[produced + i] = destination[produced - offset + i];
            produced += match;
        }
        throw StorageFormat.Corrupt("compressed block lacks its final literal sequence");
    }
    private static int Length(ReadOnlySpan<byte> source, ref int position, int nibble, int maximum)
    {
        var result = nibble;
        if (result > maximum) throw StorageFormat.Corrupt("compressed run exceeds declared length");
        if (nibble != 15) return result;
        byte next;
        do
        {
            if (position == source.Length) throw StorageFormat.Corrupt("truncated compressed length");
            next = source[position++];
            if (next > maximum - result) throw StorageFormat.Corrupt("compressed run exceeds declared length");
            result += next;
        } while (next == 255);
        return result;
    }
}
