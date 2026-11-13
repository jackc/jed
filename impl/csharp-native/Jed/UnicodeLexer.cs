using System.Text;

namespace Jed;

internal sealed partial class Lexer
{
    private Token UnicodeQuoted()
    {
        position += 2; var quote = At(); var raw = Quoted(quote, false, false).Text;
        SkipTrivia();
        var escape = '\\';
        if (sql.AsSpan(position).StartsWith("uescape", StringComparison.OrdinalIgnoreCase) &&
            !NameStart(At(7)) && !Digit(At(7)) && At(7) != '$')
        {
            position += 7; SkipTrivia();
            if (At() != '\'') throw Errors.Syntax("UESCAPE requires a simple string literal");
            var value = Quoted('\'', false).Text;
            if (value.Length != 1 || value[0] > 127 || Hex(value[0]) >= 0 || value[0] is '+' or '\'' or '"' || Space(value[0]))
                throw Errors.Syntax("invalid Unicode escape character");
            escape = value[0];
        }
        var output = new StringBuilder(); var pendingHigh = 0;
        for (var i = 0; i < raw.Length; i++)
        {
            if (raw[i] != escape)
            {
                if (pendingHigh != 0) throw Errors.Syntax("invalid Unicode surrogate pair");
                output.Append(raw[i]); continue;
            }
            if (++i >= raw.Length) throw Errors.Syntax("invalid Unicode escape");
            if (raw[i] == escape)
            {
                if (pendingHigh != 0) throw Errors.Syntax("invalid Unicode surrogate pair");
                output.Append(escape); continue;
            }
            var digits = raw[i] == '+' ? 6 : 4;
            if (digits == 6) i++;
            if (i + digits > raw.Length) throw Errors.Syntax("invalid Unicode escape");
            var scalar = 0;
            for (var j = 0; j < digits; j++)
            {
                var digit = Hex(raw[i + j]);
                if (digit < 0) throw Errors.Syntax("invalid Unicode escape");
                scalar = scalar * 16 + digit;
            }
            i += digits - 1;
            if (scalar == 0 || scalar > 0x10ffff) throw Errors.Syntax("invalid Unicode escape value");
            if (pendingHigh != 0)
            {
                if (scalar is < 0xdc00 or > 0xdfff) throw Errors.Syntax("invalid Unicode surrogate pair");
                scalar = 0x10000 + ((pendingHigh - 0xd800) << 10) + scalar - 0xdc00; pendingHigh = 0;
            }
            else if (scalar is >= 0xd800 and <= 0xdbff) { pendingHigh = scalar; continue; }
            else if (scalar is >= 0xdc00 and <= 0xdfff) throw Errors.Syntax("invalid Unicode surrogate pair");
            output.Append(char.ConvertFromUtf32(scalar));
        }
        if (pendingHigh != 0) throw Errors.Syntax("invalid Unicode surrogate pair");
        var decoded = output.ToString();
        if (quote == '"' && TextEncoding.ByteCount(decoded) > 63) throw new SqlException(SqlStates.NameTooLong, "identifier exceeds 63 bytes");
        return new(quote == '"' ? TokenKind.QuotedName : TokenKind.String, decoded);
    }
    private static int Hex(char c) => c is >= '0' and <= '9' ? c - '0' : c is >= 'a' and <= 'f' ? c - 'a' + 10 : c is >= 'A' and <= 'F' ? c - 'A' + 10 : -1;
}
