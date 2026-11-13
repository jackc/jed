using System.Globalization;

namespace Jed;

internal readonly record struct NumericLiteral(string Text, bool IsDecimal, int End);

/// <summary>ASCII numeric input and SQL token scanning, independent of host numeric parsers.</summary>
internal static class IntegerInput
{
    internal static long ParseText(string text, SqlType type)
    {
        if (!type.IsInteger()) throw new ArgumentException("integer input needs an integer target", nameof(type));
        var position = 0;
        while (position < text.Length && Space(text[position])) position++;
        var negative = position < text.Length && text[position] == '-';
        if (position < text.Length && text[position] is '+' or '-') position++;
        var radix = Radix(text, position);
        if (radix != 10) position += 2;
        var first = position;
        var minimum = SqlTypes.Fact(type).Min;
        var limit = (ulong)(-(minimum + 1)) + 1;
        ulong magnitude = 0;
        var sawDigit = false;
        while (position < text.Length)
        {
            var digit = DigitValue(text[position], radix);
            if (digit >= 0)
            {
                // PostgreSQL detects this overflow DURING the scan, before junk.
                // The last digit may cross the signed bound; that check comes
                // after syntax, preserving e.g. 0x80000000x -> 22P02 for i32.
                if (magnitude > limit / (uint)radix) throw Errors.Overflow();
                magnitude = magnitude * (uint)radix + (uint)digit;
                sawDigit = true;
                position++;
            }
            else if (text[position] == '_')
            {
                if (radix == 10 && position == first || position + 1 == text.Length || DigitValue(text[position + 1], radix) < 0)
                    throw Invalid();
                position++;
            }
            else break;
        }
        if (!sawDigit) throw Invalid();
        while (position < text.Length && Space(text[position])) position++;
        if (position != text.Length) throw Invalid();
        if (magnitude > (negative ? limit : limit - 1)) throw Errors.Overflow();
        if (!negative || magnitude == 0) return (long)magnitude;
        return -(long)(magnitude - 1) - 1;
    }

    /// <summary>Normalize one unsigned integer SQL token to decimal digits, allowing magnitude 2^63.</summary>
    internal static string NormalizeLiteral(string token)
    {
        if (token.Length == 0 || !Digit(token[0])) throw Errors.Syntax("invalid integer literal");
        var radix = Radix(token, 0);
        var start = radix == 10 ? 0 : 2;
        var end = DigitRunEnd(token, start, radix);
        if (end != token.Length || end == start) throw Errors.Syntax("invalid integer literal");
        ulong magnitude = 0;
        const ulong limit = 1UL << 63;
        for (var i = start; i < end; i++)
        {
            if (token[i] == '_') continue;
            var digit = (uint)DigitValue(token[i], radix);
            if (magnitude > (limit - digit) / (uint)radix) throw Errors.Syntax("integer literal magnitude exceeds 2^63");
            magnitude = magnitude * (uint)radix + digit;
        }
        return magnitude.ToString(CultureInfo.InvariantCulture);
    }

    /// <summary>Scan a SQL number, returning its normalized token and UTF-16 end offset.</summary>
    internal static NumericLiteral ScanLiteral(string sql, int start)
    {
        if (start < 0 || start >= sql.Length) throw new ArgumentOutOfRangeException(nameof(start));
        char At(int index) => index < sql.Length ? sql[index] : '\0';
        if (!Digit(At(start)) && !(At(start) == '.' && Digit(At(start + 1)))) throw Errors.Syntax("invalid numeric literal");
        var radix = Radix(sql, start);
        if (radix != 10)
        {
            var end = DigitRunEnd(sql, start + 2, radix);
            if (end == start + 2 || IdentifierStart(At(end)) || Digit(At(end))) throw Errors.Syntax("invalid radix integer literal");
            return new(NormalizeLiteral(sql[start..end]), false, end);
        }

        var position = Digit(At(start)) ? DigitRunEnd(sql, start, 10) : start;
        var isDecimal = false;
        // Numericfail in PostgreSQL scan.l: 1.. is an integer followed by .. .
        if (At(position) == '.' && At(position + 1) != '.')
        {
            isDecimal = true;
            position++;
            if (Digit(At(position))) position = DigitRunEnd(sql, position, 10);
        }
        if (At(position) is 'e' or 'E')
        {
            isDecimal = true;
            position++;
            if (At(position) is '+' or '-') position++;
            if (!Digit(At(position))) throw Errors.Syntax("missing exponent digits");
            position = DigitRunEnd(sql, position, 10);
        }
        if (IdentifierStart(At(position))) throw Errors.Syntax("trailing junk after numeric literal");
        var written = sql[start..position];
        return new(isDecimal ? written.Replace("_", "", StringComparison.Ordinal) : NormalizeLiteral(written), isDecimal, position);
    }

    private static int DigitRunEnd(string text, int position, int radix)
    {
        while (position < text.Length)
        {
            if (DigitValue(text[position], radix) >= 0) position++;
            else if (text[position] == '_' && position + 1 < text.Length && DigitValue(text[position + 1], radix) >= 0) position += 2;
            else break;
        }
        return position;
    }

    private static int Radix(string text, int position) => position + 1 < text.Length && text[position] == '0' ? text[position + 1] switch
    {
        'x' or 'X' => 16, 'o' or 'O' => 8, 'b' or 'B' => 2, _ => 10
    } : 10;
    private static int DigitValue(char c, int radix)
    {
        var value = c is >= '0' and <= '9' ? c - '0' : c is >= 'a' and <= 'f' ? c - 'a' + 10 : c is >= 'A' and <= 'F' ? c - 'A' + 10 : -1;
        return value < radix ? value : -1;
    }
    private static bool Digit(char c) => c is >= '0' and <= '9';
    private static bool Space(char c) => c is ' ' or '\t' or '\n' or '\r' or '\v' or '\f';
    private static bool IdentifierStart(char c) => c is >= 'a' and <= 'z' or >= 'A' and <= 'Z' or '_' or >= '\u0080';
    private static SqlException Invalid() => new(SqlStates.InvalidTextRepresentation, "invalid integer input");
}
