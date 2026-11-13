using System.Globalization;
using System.Text;

namespace Jed;

internal readonly record struct PathToken(string Kind, string Text);
internal static class JsonPathTokens
{
    internal static List<PathToken> Scan(string input)
    {
        var tokens = new List<PathToken>(); var position = 0;
        SqlException Syntax() => new("42601", "syntax error in jsonpath");
        bool Space(char value) => value is ' ' or '\t' or '\n' or '\r' or '\f';
        void Comment()
        {
            var end = input.IndexOf("*/", position + 2, StringComparison.Ordinal); if (end < 0) throw Syntax(); position = end + 2;
        }
        while (position < input.Length)
        {
            if (Space(input[position])) { position++; continue; }
            if (input.AsSpan(position).StartsWith("/*", StringComparison.Ordinal)) { Comment(); continue; }
            var first = input[position];
            if (first == '$' && position + 1 < input.Length && !Space(input[position + 1]) && (!"?%$.[]{}()|&!=<>@#,*:-+/".Contains(input[position + 1]) || input[position + 1] == '"'))
            {
                position++; var variable = new StringBuilder();
                if (input[position] == '"')
                {
                    position++; var closed = false;
                    while (position < input.Length) { if (input[position] == '"') { position++; closed = true; break; } Append(variable); }
                    if (!closed) throw Syntax();
                }
                else while (position < input.Length && !Space(input[position]) && !"?%$.[]{}()|&!=<>@#,*:-+/\"".Contains(input[position])) variable.Append(input[position++]);
                tokens.Add(new("variable", variable.ToString())); continue;
            }
            if (char.IsAsciiDigit(first))
            {
                var end = position;
                while (end < input.Length && !Space(input[end]) && !"?%$.[]{}()|&!=<>@#,*:-+/\"".Contains(input[end])) end++;
                var prefix = position; var radix = position + 1 < end && first == '0' && input[position + 1] is 'x' or 'X' or 'b' or 'B' or 'o' or 'O';
                if (radix) { prefix += 2; while (prefix < end && (char.IsAsciiHexDigit(input[prefix]) || input[prefix] == '_')) prefix++; }
                else
                {
                    while (prefix < end && (char.IsAsciiDigit(input[prefix]) || input[prefix] == '_' && prefix + 1 < end && char.IsAsciiDigit(input[prefix + 1]))) prefix++;
                    if (prefix < end && input[prefix] is 'e' or 'E')
                    {
                        var exponent = prefix + 1; while (exponent < end && char.IsAsciiDigit(input[exponent])) exponent++;
                        if (exponent > prefix + 1) prefix = exponent;
                    }
                }
                if (TextEncoding.ByteCount(input[position..end]) > TextEncoding.ByteCount(input[position..prefix]) + (radix ? 0 : 1))
                {
                    var numericName = new StringBuilder(); while (position < end) Append(numericName);
                    if (input.AsSpan(position).StartsWith("/*", StringComparison.Ordinal)) Comment();
                    else tokens.Add(new("name", numericName.ToString()));
                    continue;
                }
            }
            if (first == '"')
            {
                position++; var word = new StringBuilder(); var closed = false;
                while (position < input.Length) { if (input[position] == '"') { position++; closed = true; break; } Append(word); }
                if (!closed) throw Syntax(); tokens.Add(new("string", word.ToString())); continue;
            }
            if (char.IsAsciiDigit(first) || first == '.' && position + 1 < input.Length && char.IsAsciiDigit(input[position + 1]))
            {
                var start = position;
                while (position < input.Length && char.IsAsciiDigit(input[position])) position++;
                if (position < input.Length && input[position] == '.') { position++; while (position < input.Length && char.IsAsciiDigit(input[position])) position++; }
                if (position < input.Length && input[position] is 'e' or 'E')
                {
                    position++; if (position < input.Length && input[position] is '+' or '-') position++;
                    var begin = position; while (position < input.Length && char.IsAsciiDigit(input[position])) position++; if (begin == position) throw Syntax();
                }
                var number = input[start..position];
                if (number.Length > 1 && number[0] == '0' && char.IsAsciiDigit(number[1])) throw Syntax();
                if (position < input.Length && !Space(input[position]) && !"?%$.[]{}()|&!=<>@#, *:-+/".Contains(input[position])) throw Syntax();
                tokens.Add(new("number", number)); continue;
            }
            if ("?%$.[{}]()|&!=<>@#,*:-+/]".Contains(first))
            {
                var two = position + 1 < input.Length ? input.Substring(position, 2) : "";
                if (two is "**" or "&&" or "||" or "==" or "!=" or "<>" or "<=" or ">=") { tokens.Add(new("symbol", two)); position += 2; }
                else { tokens.Add(new("symbol", first.ToString())); position++; }
                continue;
            }
            var name = new StringBuilder(); var discarded = false;
            while (position < input.Length && !Space(input[position]) && !"?%$.[]{}()|&!=<>@#,*:-+/\"".Contains(input[position])) Append(name);
            if (input.AsSpan(position).StartsWith("/*", StringComparison.Ordinal)) { Comment(); discarded = true; }
            if (!discarded) { if (name.Length == 0) throw Syntax(); tokens.Add(new("name", name.ToString())); }
        }
        return tokens;

        void Append(StringBuilder output)
        {
            var next = input[position++];
            if (next != '\\') { if (next == '\0') throw new SqlException("22P05", "unsupported Unicode escape sequence"); output.Append(next); return; }
            if (position == input.Length) throw Syntax(); next = input[position++];
            if (next is not ('u' or 'x')) { output.Append(next switch { 'b' => '\b', 'f' => '\f', 'n' => '\n', 'r' => '\r', 't' => '\t', 'v' => '\v', _ => next }); return; }
            var codes = new List<int>();
            while (true)
            {
                var code = 0; var digits = next == 'x' ? 2 : 4;
                if (next == 'u' && position < input.Length && input[position] == '{')
                {
                    position++; var begin = position;
                    while (position < input.Length && input[position] != '}') { if (position - begin == 6) throw Syntax(); code = code * 16 + Hex(input[position++]); }
                    if (position == begin || position == input.Length) throw Syntax(); position++;
                }
                else for (var i = 0; i < digits; i++) { if (position == input.Length) throw Syntax(); code = code * 16 + Hex(input[position++]); }
                if (code > 0x10ffff) throw Syntax(); codes.Add(code);
                if (next == 'x' || position + 1 >= input.Length || input[position] != '\\' || input[position + 1] != 'u') break;
                position += 2; next = 'u';
            }
            for (var i = 0; i < codes.Count; i++)
            {
                var code = codes[i]; if (code == 0) throw new SqlException("22P05", "unsupported Unicode escape sequence");
                if (code is >= 0xd800 and <= 0xdbff)
                {
                    if (i + 1 == codes.Count || codes[i + 1] is < 0xdc00 or > 0xdfff) throw new SqlException("22P02", "invalid input syntax for type jsonpath");
                    code = 0x10000 + ((code - 0xd800) << 10) + codes[++i] - 0xdc00;
                }
                else if (code is >= 0xdc00 and <= 0xdfff) throw new SqlException("22P02", "invalid input syntax for type jsonpath");
                output.Append(char.ConvertFromUtf32(code));
            }
        }
        int Hex(char ch) => ch is >= '0' and <= '9' ? ch - '0' : ch is >= 'a' and <= 'f' ? ch - 'a' + 10 : ch is >= 'A' and <= 'F' ? ch - 'A' + 10 : throw Syntax();
    }
}
