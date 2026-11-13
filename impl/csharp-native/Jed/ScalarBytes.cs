using System.Text;

namespace Jed;

internal static class Bytea
{
    private const string HexDigits = "0123456789abcdef";
    private const string Base64Digits = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    internal static byte[] Parse(string text) => text.StartsWith("\\x", StringComparison.Ordinal)
        ? DecodeHex(text.AsSpan(2), SqlStates.InvalidTextRepresentation) : DecodeEscape(text);
    internal static string Render(ReadOnlySpan<byte> bytes) => "\\x" + EncodeHex(bytes);

    internal static string Encode(ReadOnlySpan<byte> bytes, string format, Action<long>? reserve = null)
    {
        switch (TextEncoding.FoldName(format))
        {
            case "hex":
                reserve?.Invoke(2L * bytes.Length);
                return EncodeHex(bytes);
            case "base64":
            {
                long length = ((long)bytes.Length + 2) / 3 * 4 + bytes.Length / 57;
                reserve?.Invoke(length);
                var chars = new char[Size(length)];
                var source = 0; var destination = 0; var column = 0;
                while (bytes.Length - source >= 3)
                {
                    var bits = (bytes[source] << 16) | (bytes[source + 1] << 8) | bytes[source + 2];
                    source += 3;
                    chars[destination++] = Base64Digits[(bits >> 18) & 63];
                    chars[destination++] = Base64Digits[(bits >> 12) & 63];
                    chars[destination++] = Base64Digits[(bits >> 6) & 63];
                    chars[destination++] = Base64Digits[bits & 63];
                    column += 4;
                    if (column == 76) { chars[destination++] = '\n'; column = 0; }
                }
                if (source < bytes.Length)
                {
                    var first = bytes[source++];
                    var hasSecond = source < bytes.Length;
                    var second = hasSecond ? bytes[source] : 0;
                    chars[destination++] = Base64Digits[first >> 2];
                    chars[destination++] = Base64Digits[((first & 3) << 4) | (second >> 4)];
                    chars[destination++] = hasSecond ? Base64Digits[(second & 15) << 2] : '=';
                    chars[destination] = '=';
                }
                return new string(chars);
            }
            case "escape":
            {
                long length = 0;
                foreach (var b in bytes) length += b == '\\' ? 2 : b == 0 || b >= 128 ? 4 : 1;
                reserve?.Invoke(length);
                var chars = new char[Size(length)]; var position = 0;
                foreach (var b in bytes)
                {
                    if (b == '\\') { chars[position++] = '\\'; chars[position++] = '\\'; }
                    else if (b == 0 || b >= 128)
                    {
                        chars[position++] = '\\'; chars[position++] = (char)('0' + (b >> 6));
                        chars[position++] = (char)('0' + ((b >> 3) & 7)); chars[position++] = (char)('0' + (b & 7));
                    }
                    else chars[position++] = (char)b;
                }
                return new string(chars);
            }
            default: throw new SqlException(SqlStates.InvalidParameterValue, $"unrecognized encoding: {format}");
        }
    }

    internal static byte[] Decode(string text, string format, Action<long>? reserve = null) => TextEncoding.FoldName(format) switch
    {
        "hex" => DecodeHex(text.AsSpan(), SqlStates.InvalidParameterValue, reserve),
        "base64" => DecodeBase64(text, reserve),
        "escape" => DecodeEscape(text, reserve),
        _ => throw new SqlException(SqlStates.InvalidParameterValue, $"unrecognized encoding: {format}")
    };

    private static string EncodeHex(ReadOnlySpan<byte> bytes)
    {
        var chars = new char[Size(2L * bytes.Length)];
        for (var i = 0; i < bytes.Length; i++) { chars[2 * i] = HexDigits[bytes[i] >> 4]; chars[2 * i + 1] = HexDigits[bytes[i] & 15]; }
        return new string(chars);
    }

    private static byte[] DecodeHex(ReadOnlySpan<char> text, string state, Action<long>? reserve = null)
    {
        // Validate/count first, so any requested growth reservation precedes
        // allocation and malformed input reports its first decoding error.
        var length = WalkHex(text, Span<byte>.Empty, false, state);
        reserve?.Invoke(length);
        var output = new byte[length];
        WalkHex(text, output, true, state);
        return output;
    }
    private static int WalkHex(ReadOnlySpan<char> text, Span<byte> output, bool write, string state)
    {
        var count = 0;
        for (var position = 0; position < text.Length;)
        {
            if (White(text[position])) { position++; continue; }
            var high = HexValue(text[position]);
            if (high < 0) throw new SqlException(state, $"invalid hexadecimal digit: \"{Character(text, position)}\"");
            position++;
            if (position == text.Length) throw new SqlException(state, "invalid hexadecimal data: odd number of digits");
            var low = HexValue(text[position]);
            if (low < 0) throw new SqlException(state, $"invalid hexadecimal digit: \"{Character(text, position)}\"");
            position++;
            if (write) output[count] = (byte)((high << 4) | low);
            count++;
        }
        return count;
    }

    private static byte[] DecodeEscape(string text, Action<long>? reserve = null)
    {
        TextEncoding.Validate(text);
        var length = WalkEscape(text, Span<byte>.Empty, false);
        reserve?.Invoke(length);
        var output = new byte[length];
        WalkEscape(text, output, true);
        return output;
    }
    private static int WalkEscape(string text, Span<byte> output, bool write)
    {
        var count = 0;
        for (var position = 0; position < text.Length;)
        {
            if (text[position] != '\\')
            {
                var rune = Rune.GetRuneAt(text, position);
                if (write) rune.EncodeToUtf8(output[count..]);
                count = checked(count + rune.Utf8SequenceLength); position += rune.Utf16SequenceLength;
            }
            else
            {
                byte value;
                if (position + 1 < text.Length && text[position + 1] == '\\') { value = (byte)'\\'; position += 2; }
                else if (position + 3 < text.Length && text[position + 1] is >= '0' and <= '3' &&
                         text[position + 2] is >= '0' and <= '7' && text[position + 3] is >= '0' and <= '7')
                {
                    value = (byte)(((text[position + 1] - '0') << 6) | ((text[position + 2] - '0') << 3) | (text[position + 3] - '0'));
                    position += 4;
                }
                else throw new SqlException(SqlStates.InvalidTextRepresentation, "invalid input syntax for type bytea");
                if (write) output[count] = value;
                count++;
            }
        }
        return count;
    }

    private static byte[] DecodeBase64(string text, Action<long>? reserve)
    {
        var count = WalkBase64(text, Span<byte>.Empty, false);
        reserve?.Invoke(count);
        var output = new byte[count];
        WalkBase64(text, output, true);
        return output;
    }
    private static int WalkBase64(string text, Span<byte> output, bool write)
    {
        var group = 0; var bits = 0; var width = 3; var count = 0; var padded = false;
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i]; if (White(c)) continue;
            int value;
            if (c == '=')
            {
                if (!padded)
                {
                    if (group < 2) throw new SqlException(SqlStates.InvalidParameterValue, "unexpected \"=\" while decoding base64 sequence");
                    width = group - 1; padded = true;
                }
                value = 0;
            }
            else
            {
                value = c < 128 ? Base64Digits.IndexOf(c) : -1;
                if (value < 0) throw new SqlException(SqlStates.InvalidParameterValue, $"invalid symbol \"{Character(text.AsSpan(), i)}\" found while decoding base64 sequence");
            }
            bits = (bits << 6) | value; group++;
            if (group == 4)
            {
                if (write)
                    for (var j = 0; j < width; j++) output[count + j] = unchecked((byte)(bits >> (16 - 8 * j)));
                count += width; group = 0; bits = 0;
                // PostgreSQL retains the width chosen by the first padded group.
            }
        }
        if (group != 0) throw new SqlException(SqlStates.InvalidParameterValue, "invalid base64 end sequence");
        return count;
    }

    internal static byte[] Substring(ReadOnlySpan<byte> bytes, long start, long? count = null)
    {
        if (count is < 0) throw new SqlException(SqlStates.SubstringError, "negative substring length not allowed");
        var first = Math.Clamp(start, 1, (long)bytes.Length + 1) - 1;
        var end = count is null ? (long)bytes.Length : Math.Clamp(SaturatingAdd(start, count.Value), 1, (long)bytes.Length + 1) - 1;
        return bytes.Slice((int)first, (int)Math.Max(0, end - first)).ToArray();
    }
    internal static long GetByte(ReadOnlySpan<byte> bytes, long index) { CheckIndex(index, bytes.Length); return bytes[(int)index]; }
    internal static byte[] SetByte(ReadOnlySpan<byte> bytes, long index, long value)
    {
        CheckIndex(index, bytes.Length); var output = bytes.ToArray(); output[(int)index] = unchecked((byte)value); return output;
    }
    internal static long GetBit(ReadOnlySpan<byte> bytes, long index)
    {
        CheckIndex(index, (long)bytes.Length * 8); return (bytes[(int)(index / 8)] >> (int)(index % 8)) & 1;
    }
    internal static byte[] SetBit(ReadOnlySpan<byte> bytes, long index, long value)
    {
        CheckIndex(index, (long)bytes.Length * 8);
        if (value is not (0 or 1)) throw new SqlException(SqlStates.InvalidParameterValue, "new bit must be 0 or 1");
        var output = bytes.ToArray(); var mask = 1 << (int)(index % 8);
        output[(int)(index / 8)] = (byte)((output[(int)(index / 8)] & ~mask) | (value == 1 ? mask : 0)); return output;
    }
    internal static byte[] Concat(ReadOnlySpan<byte> first, ReadOnlySpan<byte> second, Action<long>? reserve = null)
    {
        var length = (long)first.Length + second.Length; reserve?.Invoke(length);
        var output = new byte[Size(length)]; first.CopyTo(output); second.CopyTo(output.AsSpan(first.Length)); return output;
    }
    internal static byte[] Overlay(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> replacement, long start, long? count = null, Action<long>? reserve = null)
    {
        if (start <= 0) throw new SqlException(SqlStates.SubstringError, "negative substring length not allowed");
        var tail = SaturatingAdd(start, count ?? replacement.Length);
        if (tail is < int.MinValue or > int.MaxValue) throw Errors.Overflow();
        var headLength = (int)Math.Min(bytes.Length, start - 1);
        var tailStart = (int)Math.Clamp(tail - 1, 0, bytes.Length);
        var length = (long)headLength + replacement.Length + bytes.Length - tailStart;
        reserve?.Invoke(length);
        var output = new byte[Size(length)];
        bytes[..headLength].CopyTo(output); replacement.CopyTo(output.AsSpan(headLength));
        bytes[tailStart..].CopyTo(output.AsSpan(headLength + replacement.Length)); return output;
    }
    internal static long Position(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> needle) => bytes.IndexOf(needle) + 1L;
    internal static byte[] Trim(ReadOnlySpan<byte> bytes, ReadOnlySpan<byte> set, bool left = true, bool right = true)
    {
        Span<bool> members = stackalloc bool[256]; members.Clear(); foreach (var b in set) members[b] = true;
        var start = 0; var end = bytes.Length;
        if (left) while (start < end && members[bytes[start]]) start++;
        if (right) while (end > start && members[bytes[end - 1]]) end--;
        return bytes[start..end].ToArray();
    }
    private static void CheckIndex(long index, long length)
    {
        if (index < 0 || index >= length) throw new SqlException(SqlStates.ArraySubscriptError, FormattableString.Invariant($"index {index} out of valid range, 0..{length - 1}"));
    }
    private static long SaturatingAdd(long left, long right) => right > 0 && left > long.MaxValue - right ? long.MaxValue :
        right < 0 && left < long.MinValue - right ? long.MinValue : left + right;
    private static int Size(long length) => length > int.MaxValue ? throw new SqlException(SqlStates.ProgramLimitExceeded, "byte string is too large") : (int)length;
    private static bool White(char c) => c is ' ' or '\t' or '\n' or '\r';
    internal static int HexValue(char c) => c is >= '0' and <= '9' ? c - '0' : c is >= 'a' and <= 'f' ? c - 'a' + 10 : c is >= 'A' and <= 'F' ? c - 'A' + 10 : -1;
    private static string Character(ReadOnlySpan<char> text, int position) => text.Slice(position, char.IsHighSurrogate(text[position]) && position + 1 < text.Length && char.IsLowSurrogate(text[position + 1]) ? 2 : 1).ToString();
}

internal static class Uuid
{
    internal static byte[] Parse(string text)
    {
        var source = text.AsSpan();
        if (source.StartsWith("{"))
        {
            if (!source.EndsWith("}")) throw Invalid();
            source = source[1..^1];
        }
        var output = new byte[16]; var position = 0;
        for (var i = 0; i < 16; i++)
        {
            if (position + 1 >= source.Length) throw Invalid();
            var high = Bytea.HexValue(source[position++]); var low = Bytea.HexValue(source[position++]);
            if (high < 0 || low < 0) throw Invalid();
            output[i] = (byte)((high << 4) | low);
            if (i < 15 && i % 2 == 1 && position < source.Length && source[position] == '-') position++;
        }
        if (position != source.Length) throw Invalid();
        return output;
    }
    internal static string Render(ReadOnlySpan<byte> bytes)
    {
        if (bytes.Length != 16) throw new ArgumentException("UUID requires 16 bytes", nameof(bytes));
        var hex = Convert.ToHexStringLower(bytes);
        return string.Concat(hex.AsSpan(0, 8), "-", hex.AsSpan(8, 4)) + "-" + hex[12..16] + "-" + hex[16..20] + "-" + hex[20..];
    }
    private static SqlException Invalid() => new(SqlStates.InvalidTextRepresentation, "invalid input syntax for type uuid");
}
