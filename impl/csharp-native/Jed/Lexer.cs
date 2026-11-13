using System.Text;

namespace Jed;

internal enum TokenKind { End, Word, QuotedName, Integer, Decimal, String, Symbol, Parameter }
internal readonly record struct Token(TokenKind Kind, string Text);

internal sealed partial class Lexer
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

    private void SkipTrivia()
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
    }

    internal Token Next()
    {
        SkipTrivia();
        if (position == sql.Length) return new(TokenKind.End, "");
        var start = position;
        var c = At();
        if ((c is 'u' or 'U') && At(1) == '&' && At(2) is '\'' or '"') return UnicodeQuoted();
        if (Digit(c) || c == '.' && Digit(At(1)))
        {
            var number = IntegerInput.ScanLiteral(sql, start);
            position = number.End;
            return new(number.IsDecimal ? TokenKind.Decimal : TokenKind.Integer, number.Text);
        }
        if ((c is 'e' or 'E') && At(1) == '\'') { position++; return Quoted('\'', true); }
        if (c == '$' && Digit(At(1)))
        {
            position++; while (Digit(At())) position++;
            var index = sql[(start + 1)..position];
            if (index[0] == '0' || !int.TryParse(index, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var number) || number is < 1 or > 65535) throw Errors.Syntax("parameter index must be between 1 and 65535");
            return new(TokenKind.Parameter, index);
        }
        if (c == '$')
        {
            position++;
            while (NameStart(At()) || Digit(At())) position++;
            if (At() != '$') throw Errors.Syntax("invalid dollar quote");
            position++; var delimiter = sql[start..position];
            var end = sql.IndexOf(delimiter, position, StringComparison.Ordinal);
            if (end < 0) throw Errors.Syntax("unterminated dollar quote");
            var body = sql[position..end]; position = end + delimiter.Length;
            return new(TokenKind.String, body);
        }
        if (NameStart(c))
        {
            position++;
            while (NameStart(At()) || Digit(At()) || At() == '$') position++;
            if (position - start > 63) throw new SqlException(SqlStates.NameTooLong, "identifier exceeds 63 bytes");
            return new(TokenKind.Word, TextEncoding.FoldName(sql[start..position]));
        }
        if (c is '\'' or '"') return Quoted(c, false);
        if (position + 2 < sql.Length && sql.Substring(position, 3) is "-|-" or "->>" or "#>>" or "!~*")
        { var triple = sql.Substring(position, 3); position += 3; return new(TokenKind.Symbol, triple); }
        if (position + 1 < sql.Length)
        {
            var pair = sql.Substring(position, 2);
            if (pair is "::" or "<=" or ">=" or "<>" or "!=" or "||" or "=>" or "<<" or ">>" or "!~" or "~*" or "@>" or "<@" or "&&" or "&<" or "&>" or "->" or "#>" or "#-" or "?|" or "?&" or "@?" or "@@")
            {
                position += 2;
                return new(TokenKind.Symbol, pair);
            }
        }
        position++;
        if ("+-*/%=<>()[],;.^&|#~:?".Contains(c)) return new(TokenKind.Symbol, c.ToString());
        throw Errors.Syntax($"unexpected character: {c}");
    }

    private Token Quoted(char c, bool escaped, bool validateName = true)
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
                else if (next == '\\' && escaped)
                {
                    if (position == sql.Length) throw Errors.Syntax("unterminated escape");
                    var escape = sql[position++];
                    switch (escape)
                    {
                        case 'n': value.Append('\n'); break;
                        case 'r': value.Append('\r'); break;
                        case 't': value.Append('\t'); break;
                        case 'b': value.Append('\b'); break;
                        case 'f': value.Append('\f'); break;
                        default: value.Append(escape); break;
                    }
                }
                else if (next == '\0') throw Errors.Syntax("NUL in SQL text");
                else value.Append(next);
            }
            if (c == '"' && value.Length == 0) throw Errors.Syntax("empty quoted identifier");
            if (validateName && c == '"' && TextEncoding.Utf8.GetByteCount(value.ToString()) > 63)
                throw new SqlException(SqlStates.NameTooLong, "identifier exceeds 63 bytes");
            return new(c == '\'' ? TokenKind.String : TokenKind.QuotedName, value.ToString());
    }
}
