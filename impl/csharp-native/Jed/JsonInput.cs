using System.Text;

namespace Jed;

internal sealed class JsonInput(string input, bool decode, bool canonical)
{
    private int position;
    internal static SqlJsonNode Parse(string input, bool decode = true, bool canonical = true)
    {
        TextEncoding.Validate(input); var parser = new JsonInput(input, decode, canonical);
        var node = parser.Value(0); parser.Spaces(); if (parser.position != input.Length) throw Invalid(); return node;
    }
    private static SqlException Invalid() => new("22P02", "invalid input syntax for type json");
    private void Spaces() { while (position < input.Length && input[position] is ' ' or '\t' or '\r' or '\n') position++; }
    private bool Take(char character) { if (position >= input.Length || input[position] != character) return false; position++; return true; }
    private SqlJsonNode Value(int depth)
    {
        Spaces(); var start = position; if (position == input.Length) throw Invalid(); var ch = input[position];
        if (ch is '[' or '{' && depth == SqlJsonNode.MaximumDepth) throw SqlJsonNode.TooDeep();
        if (Take('['))
        {
            var elements = new List<SqlJsonNode>(); Spaces();
            if (!Take(']'))
                while (true) { elements.Add(Value(depth + 1)); Spaces(); if (Take(']')) break; if (!Take(',')) throw Invalid(); }
            return new(SqlJsonKind.Array, elements: elements.ToArray(), source: input, start: start, length: position - start);
        }
        if (Take('{'))
        {
            var members = new List<SqlJsonMember>(); Spaces();
            if (!Take('}'))
                while (true)
                {
                    Spaces(); var key = String(); Spaces(); if (!Take(':')) throw Invalid(); members.Add(new(key, Value(depth + 1))); Spaces(); if (Take('}')) break; if (!Take(',')) throw Invalid();
                }
            return new(SqlJsonKind.Object, members: canonical ? SqlJsonNode.Canonicalize(members) : members.ToArray(), source: input, start: start, length: position - start);
        }
        if (ch == '"') return new(SqlJsonKind.String, text: String(), source: input, start: start, length: position - start);
        foreach (var literal in new[] { ("null", SqlJsonKind.Null), ("false", SqlJsonKind.False), ("true", SqlJsonKind.True) })
            if (input.AsSpan(position).StartsWith(literal.Item1, StringComparison.Ordinal)) { position += literal.Item1.Length; return new(literal.Item2, source: input, start: start, length: position - start); }
        if (ch != '-' && !char.IsAsciiDigit(ch)) throw Invalid();
        Take('-');
        if (Take('0')) { if (position < input.Length && char.IsAsciiDigit(input[position])) throw Invalid(); }
        else { if (position == input.Length || input[position] is < '1' or > '9') throw Invalid(); Digits(); }
        if (Take('.')) { var begin = position; Digits(); if (begin == position) throw Invalid(); }
        if (Take('e') || Take('E')) { if (!Take('+')) Take('-'); var begin = position; Digits(); if (begin == position) throw Invalid(); }
        var raw = input.Substring(start, position - start);
        return new(SqlJsonKind.Number, number: decode ? ExactDecimal.Parse(raw) : null, text: raw, source: input, start: start, length: position - start);
    }
    private void Digits() { while (position < input.Length && char.IsAsciiDigit(input[position])) position++; }
    private string String()
    {
        if (!Take('"')) throw Invalid(); var output = new StringBuilder();
        while (position < input.Length)
        {
            var ch = input[position++]; if (ch == '"') return output.ToString(); if (ch < 32) throw Invalid();
            if (ch != '\\') { output.Append(ch); continue; }
            if (position == input.Length) throw Invalid(); var escaped = input[position++];
            switch (escaped)
            {
                case '"': case '\\': case '/': output.Append(escaped); break;
                case 'b': output.Append('\b'); break; case 'f': output.Append('\f'); break; case 'n': output.Append('\n'); break; case 'r': output.Append('\r'); break; case 't': output.Append('\t'); break;
                case 'u':
                    var code = Hex4(); if (!decode) { output.Append('x'); break; }
                    if (code == 0) throw new SqlException("22P05", "unsupported Unicode escape sequence");
                    if (code is >= 0xd800 and <= 0xdbff)
                    {
                        if (!Take('\\') || !Take('u')) throw Invalid(); var low = Hex4(); if (low is < 0xdc00 or > 0xdfff) throw Invalid();
                        output.Append(char.ConvertFromUtf32(0x10000 + ((code - 0xd800) << 10) + low - 0xdc00));
                    }
                    else { if (code is >= 0xdc00 and <= 0xdfff) throw Invalid(); output.Append((char)code); }
                    break;
                default: throw Invalid();
            }
        }
        throw Invalid();
    }
    private int Hex4()
    {
        if (position + 4 > input.Length) throw Invalid(); var number = 0;
        for (var i = 0; i < 4; i++) { var ch = input[position++]; var digit = ch is >= '0' and <= '9' ? ch - '0' : ch is >= 'a' and <= 'f' ? ch - 'a' + 10 : ch is >= 'A' and <= 'F' ? ch - 'A' + 10 : -1; if (digit < 0) throw Invalid(); number = number * 16 + digit; }
        return number;
    }
}
