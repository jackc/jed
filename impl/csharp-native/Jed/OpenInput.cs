using System.Globalization;
using System.Text;

namespace Jed;

internal static class OpenInput
{
    internal static SqlValue Parse(string text, SqlType type, int depth = 0, TemporalContext? context = null)
    {
        if (depth > 32) throw new SqlException(SqlStates.StatementTooComplex, "composite nesting limit exceeded");
        return type.Kind switch
        {
            SqlKind.Array => SqlValue.Array(ParseArray(text, type.Element!, depth, context)),
            SqlKind.Range => SqlValue.Range(ParseRange(text, type, context)),
            SqlKind.Composite => SqlValue.Composite(ParseComposite(text, type, depth + 1, context)),
            _ => context is null ? Resolve.ParseText(text, type) : TemporalText.Parse(text, type, context)
        };
    }
    private static SqlException Invalid() => new(SqlStates.InvalidTextRepresentation, "malformed container input");
    private static bool White(char c) => c is ' ' or '\t' or '\n' or '\r' or '\f' or '\v';
    private static string Trim(string text) => text.Trim(' ', '\t', '\n', '\r', '\f', '\v');
    private static string ElementText(SqlValue value) => value.Type == SqlType.Boolean ? value.AsBoolean() ? "t" : "f" : value.ToString();
    private static string Quote(string text, string special, bool backslash, bool nullToken = false)
    {
        if (text.Length != 0 && !(nullToken && text.Equals("NULL", StringComparison.OrdinalIgnoreCase)) && !text.Any(c => White(c) || special.Contains(c))) return text;
        return "\"" + text.Replace("\\", "\\\\", StringComparison.Ordinal).Replace("\"", backslash ? "\\\"" : "\"\"", StringComparison.Ordinal) + "\"";
    }
    internal static string RenderRange(SqlRange range, Func<SqlValue, string>? render = null)
    {
        if (range.IsEmpty) return "empty";
        var lower = range.Lower is { } lo ? Quote(render is null ? ElementText(lo) : render(lo), ",[]()\"\\", false) : "";
        var upper = range.Upper is { } hi ? Quote(render is null ? ElementText(hi) : render(hi), ",[]()\"\\", false) : "";
        return (range.LowerInclusive ? "[" : "(") + lower + "," + upper + (range.UpperInclusive ? "]" : ")");
    }
    internal static string RenderComposite(SqlComposite composite, Func<SqlValue, string>? render = null) => "(" + string.Join(',', composite.Fields.Select(value => value.IsNull ? "" : Quote(render is null ? ElementText(value) : render(value), ",()\"\\", false))) + ")";
    internal static string RenderArray(SqlArray array, Func<SqlValue, string>? render = null)
    {
        if (array.Count == 0) return "{}";
        var output = new StringBuilder();
        if (array.LowerBounds.Any(bound => bound != 1))
        {
            for (var d = 0; d < array.Rank; d++)
                output.Append('[').Append(array.LowerBounds[d].ToString(CultureInfo.InvariantCulture)).Append(':').Append(((long)array.LowerBounds[d] + array.Dimensions[d] - 1).ToString(CultureInfo.InvariantCulture)).Append(']');
            output.Append('=');
        }
        var element = 0;
        void Dimension(int dimension)
        {
            output.Append('{');
            for (var index = 0; index < array.Dimensions[dimension]; index++)
            {
                if (index != 0) output.Append(',');
                if (dimension + 1 < array.Rank) Dimension(dimension + 1);
                else
                {
                    var value = array.Values[element++];
                    output.Append(value.IsNull ? "NULL" : Quote(render is null ? ElementText(value) : render(value), ",{}\"\\", true, true));
                }
            }
            output.Append('}');
        }
        Dimension(0);
        return output.ToString();
    }
    private readonly record struct Token(string Text, bool Quoted);
    // Shared record/range field framing. Array's nested brace grammar is separate.
    private static List<Token> Tokens(string text, char closing, bool nested)
    {
        var tokens = new List<Token>();
        var current = new StringBuilder();
        var quoted = false; var sawQuote = false; var depth = 0;
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (c == '\\')
            {
                if (++i == text.Length) throw Invalid();
                current.Append(text[i]); sawQuote = true; continue;
            }
            if (c == '"')
            {
                sawQuote = true;
                if (quoted && i + 1 < text.Length && text[i + 1] == '"') { current.Append('"'); i++; }
                else quoted = !quoted;
                continue;
            }
            if (!quoted)
            {
                if (nested && c is '(' or '{' or '[') depth++;
                if (nested && c is ')' or '}' or ']') { if (--depth < 0) throw Invalid(); }
                if (c == ',' && depth == 0)
                {
                    tokens.Add(new(current.ToString(), sawQuote)); current.Clear(); sawQuote = false; continue;
                }
                if (!nested && c == closing) throw Invalid();
            }
            current.Append(c);
        }
        if (quoted || depth != 0) throw Invalid();
        tokens.Add(new(current.ToString(), sawQuote));
        return tokens;
    }
    private static SqlRange ParseRange(string text, SqlType type, TemporalContext? context)
    {
        text = Trim(text);
        if (text.Equals("empty", StringComparison.OrdinalIgnoreCase)) return SqlRange.Empty(type);
        if (text.Length < 3 || text[0] is not ('[' or '(') || text[^1] is not (']' or ')')) throw Invalid();
        var fields = Tokens(text[1..^1], text[^1], false);
        if (fields.Count != 2) throw Invalid();
        SqlValue? Bound(Token token)
        {
            if (token.Text.Length == 0 && !token.Quoted) return null;
            try { return context is null ? Resolve.ParseText(token.Text, type.Element!) : TemporalText.Parse(token.Text, type.Element!, context); }
            catch (SqlException) { throw Invalid(); }
        }
        return new(type, Bound(fields[0]), Bound(fields[1]), text[0] == '[', text[^1] == ']');
    }
    private static SqlComposite ParseComposite(string text, SqlType type, int depth, TemporalContext? context)
    {
        text = Trim(text);
        if (text.Length < 2 || text[0] != '(' || text[^1] != ')') throw Invalid();
        var tokens = Tokens(text[1..^1], ')', true);
        if (tokens.Count != type.Fields.Count) throw Invalid();
        var values = new SqlValue[tokens.Count];
        for (var i = 0; i < values.Length; i++)
        {
            var field = type.Fields[i];
            values[i] = tokens[i].Text.Length == 0 && !tokens[i].Quoted ? SqlValue.Null(field.Type) : field.Type == SqlType.Interval ? SqlValue.Interval(IntervalInput.Parse(tokens[i].Text, field.IntervalFields, field.Precision)) : Parse(tokens[i].Text, field.Type, depth, context);
            if (values[i].IsNull && field.NotNull) throw new SqlException(SqlStates.NotNullViolation, "null composite field");
            if (!values[i].IsNull)
            {
                values[i] = field.Type == SqlType.Interval ? SqlValue.Interval(IntervalInput.Adjust(values[i].AsInterval(), field.IntervalFields, field.Precision))
                    : Resolve.ApplyModifier(values[i], new(field.Type, field.Precision, field.Scale, field.Length));
            }
        }
        return new(type, values);
    }
    private static SqlArray ParseArray(string text, SqlType element, int depth, TemporalContext? context)
    {
        var parser = new ArrayParser(text, element, depth, context);
        return parser.Parse();
    }
    private sealed class ArrayParser(string text, SqlType element, int compositeDepth, TemporalContext? context)
    {
        private int position;
        private readonly List<SqlValue> values = [];
        private void Space() { while (position < text.Length && White(text[position])) position++; }
        private void Expect(char c) { if (position == text.Length || text[position++] != c) throw Invalid(); }
        internal SqlArray Parse()
        {
            Space();
            var lengths = new List<int>(); var bounds = new List<int>();
            while (position < text.Length && text[position] == '[')
            {
                if (lengths.Count == 6) throw Invalid();
                position++; Space(); var lower = Bound(); Space(); Expect(':'); Space(); var upper = Bound(); Space(); Expect(']'); Space();
                if (upper < lower) throw new SqlException(SqlStates.ArraySubscriptError, "array upper bound is smaller than lower bound");
                if (upper == int.MaxValue || (long)upper - lower + 1 > int.MaxValue) throw new SqlException(SqlStates.ProgramLimitExceeded, "array bound is too large");
                lengths.Add((int)((long)upper - lower + 1)); bounds.Add(lower);
            }
            if (lengths.Count != 0) { Expect('='); Space(); }
            var dimensions = Dimension(0);
            Space(); if (position != text.Length) throw Invalid();
            if (lengths.Count != 0 && !lengths.SequenceEqual(dimensions)) throw Invalid();
            return new(element, values, dimensions, lengths.Count == 0 ? null : bounds);
        }
        private int Bound()
        {
            var start = position;
            if (position < text.Length && text[position] is '+' or '-') position++;
            var digits = position;
            while (position < text.Length && text[position] is >= '0' and <= '9') position++;
            if (position == digits) throw Invalid();
            if (!int.TryParse(text.AsSpan(start, position - start), NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var bound))
                throw new SqlException(SqlStates.ProgramLimitExceeded, "array bound is outside integer range");
            return bound;
        }
        private int[] Dimension(int nesting)
        {
            if (nesting == 6) throw Invalid();
            Expect('{'); Space();
            if (position < text.Length && text[position] == '}') { position++; return []; }
            var nested = position < text.Length && text[position] == '{';
            int[]? childShape = null;
            var length = 0;
            while (true)
            {
                Space();
                if (nested)
                {
                    var shape = Dimension(nesting + 1);
                    if (shape.Length == 0) throw Invalid();
                    if (childShape is null) childShape = shape;
                    else if (!childShape.SequenceEqual(shape)) throw Invalid();
                }
                else
                {
                    var token = ReadToken();
                    values.Add(!token.Quoted && token.Text.Equals("NULL", StringComparison.OrdinalIgnoreCase) ? SqlValue.Null(element) : OpenInput.Parse(token.Text, element, compositeDepth, context));
                }
                length++; Space();
                if (position == text.Length) throw Invalid();
                var delimiter = text[position++];
                if (delimiter == '}') break;
                if (delimiter != ',') throw Invalid();
            }
            return childShape is null ? [length] : [length, .. childShape];
        }
        private Token ReadToken()
        {
            var output = new StringBuilder(); var quoted = false;
            if (position == text.Length) throw Invalid();
            if (text[position] == '"')
            {
                quoted = true; position++;
                while (position < text.Length)
                {
                    var c = text[position++];
                    if (c == '"') { Space(); return new(output.ToString(), true); }
                    if (c == '\\') { if (position == text.Length) throw Invalid(); c = text[position++]; }
                    output.Append(c);
                }
                throw Invalid();
            }
            var significant = 0;
            while (position < text.Length && text[position] is not (',' or '}'))
            {
                var c = text[position++];
                if (c is '{' or '"') throw Invalid();
                if (c == '\\')
                {
                    if (position == text.Length) throw Invalid();
                    output.Append(text[position++]); quoted = true; significant = output.Length;
                }
                else { output.Append(c); if (!White(c)) significant = output.Length; }
            }
            if (significant == 0) throw Invalid();
            return new(output.ToString(0, significant), quoted);
        }
    }
}
