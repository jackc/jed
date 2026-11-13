using System.Text;

namespace Jed;

internal enum TokenKind { End, Word, QuotedName, Integer, String, Symbol }
internal readonly record struct Token(TokenKind Kind, string Text);

internal sealed class Lexer
{
    private readonly string sql;
    private int position;
    internal Lexer(string sql)
    {
        TextEncoding.Validate(sql);
        this.sql = sql;
    }
    private char At(int offset = 0) => position + offset < sql.Length ? sql[position + offset] : '\0';
    private static bool Space(char c) => c is ' ' or '\t' or '\r' or '\n' or '\f' or '\v';
    private static bool Digit(char c) => c is >= '0' and <= '9';
    private static bool NameStart(char c) => c is >= 'a' and <= 'z' or >= 'A' and <= 'Z' or '_';

    internal Token Next()
    {
        while (position < sql.Length)
        {
            if (Space(At())) { position++; continue; }
            if (At() == '-' && At(1) == '-')
            {
                while (position < sql.Length && At() != '\n') position++;
                continue;
            }
            if (At() == '/' && At(1) == '*')
            {
                position += 2;
                var nesting = 1;
                while (nesting > 0)
                {
                    if (position >= sql.Length) throw Errors.Syntax("unterminated block comment");
                    if (At() == '/' && At(1) == '*') { nesting++; position += 2; }
                    else if (At() == '*' && At(1) == '/') { nesting--; position += 2; }
                    else position++;
                }
                continue;
            }
            break;
        }
        if (position == sql.Length) return new(TokenKind.End, "");
        var start = position;
        var c = At();
        if (Digit(c))
        {
            while (Digit(At())) position++;
            if (At() is '.' or 'e' or 'E') throw Errors.Unsupported("decimal literals");
            if (!ulong.TryParse(sql.AsSpan(start, position - start), System.Globalization.NumberStyles.None,
                                System.Globalization.CultureInfo.InvariantCulture, out var magnitude) || magnitude > (1UL << 63))
                throw Errors.Syntax("integer literal magnitude exceeds 2^63");
            return new(TokenKind.Integer, sql[start..position]);
        }
        if (NameStart(c))
        {
            position++;
            while (NameStart(At()) || Digit(At()) || At() == '$') position++;
            if (position - start > 63) throw new SqlException(SqlStates.NameTooLong, "identifier exceeds 63 bytes");
            return new(TokenKind.Word, TextEncoding.FoldName(sql[start..position]));
        }
        if (c is '\'' or '"')
        {
            position++;
            var value = new StringBuilder();
            while (true)
            {
                if (position == sql.Length) throw Errors.Syntax("unterminated quoted token");
                var next = sql[position++];
                if (next == c)
                {
                    if (At() == c) { value.Append(c); position++; }
                    else break;
                }
                else if (next == '\0') throw Errors.Syntax("NUL in SQL text");
                else value.Append(next);
            }
            if (c == '"' && value.Length == 0) throw Errors.Syntax("empty quoted identifier");
            if (c == '"' && TextEncoding.Utf8.GetByteCount(value.ToString()) > 63)
                throw new SqlException(SqlStates.NameTooLong, "identifier exceeds 63 bytes");
            return new(c == '\'' ? TokenKind.String : TokenKind.QuotedName, value.ToString());
        }
        if (position + 1 < sql.Length)
        {
            var pair = sql.Substring(position, 2);
            if (pair is "::" or "<=" or ">=" or "<>" or "!=" or "||")
            {
                position += 2;
                return new(TokenKind.Symbol, pair);
            }
        }
        position++;
        if ("+-*/%=<>()[],;".Contains(c)) return new(TokenKind.Symbol, c.ToString());
        throw Errors.Syntax($"unexpected character: {c}");
    }
}
