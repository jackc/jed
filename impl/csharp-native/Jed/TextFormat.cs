using System.Text;

namespace Jed;

internal static class TextFormat
{
    internal static string Identifier(string text)
    {
        var bare = text.Length != 0 && (text[0] is >= 'a' and <= 'z' or '_') && text.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '_');
        return bare && Array.BinarySearch(QuotedKeywords.Values, text, StringComparer.Ordinal) < 0 ? text : "\"" + text.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";
    }
    private static string Literal(string text) => (text.Contains('\\') ? "E" : "") + "'" + text.Replace("\\", "\\\\", StringComparison.Ordinal).Replace("'", "''", StringComparison.Ordinal) + "'";
    internal static SqlValue Format(SqlValue[] values, Meter meter)
    {
        if (values[0].IsNull) return SqlValue.Null(SqlType.Text);
        var format = values[0].AsText(); meter.Charge(TextEncoding.ByteCount(format) * Costs.ScalarByte); meter.Guard();
        var pieces = new List<(string Text, int Padding, bool Left)>(); var at = 0; var next = 1; long bytes = 0;
        void Piece(string text, int padding = 0, bool left = false) { bytes += TextEncoding.ByteCount(text) + (long)padding; pieces.Add((text, padding, left)); }
        int Number()
        {
            long value = 0;
            while (at < format.Length && format[at] is >= '0' and <= '9')
            { value = value * 10 + format[at++] - '0'; if (value > int.MaxValue) throw Errors.Overflow(); }
            return (int)value;
        }
        SqlValue Argument(int position)
        {
            if (position == 0) throw Bad("format specifies argument 0, but arguments are numbered from 1");
            if (position >= values.Length) throw Bad("too few arguments for format()"); next = position + 1; return values[position];
        }
        string Render(SqlValue value) => value.Type == SqlType.Boolean ? value.AsBoolean() ? "t" : "f" : TemporalRender.CastText(value, meter);
        while (at < format.Length)
        {
            var start = at;
            while (at < format.Length && format[at] != '%') at++;
            if (at > start) Piece(format[start..at]); if (at == format.Length) break;
            at++;
            if (at < format.Length && format[at] == '%') { at++; Piece("%"); continue; }
            var position = -1; var width = 0; var left = false; var widthRead = false;
            if (at < format.Length && format[at] is >= '0' and <= '9')
            {
                var number = Number();
                if (at < format.Length && format[at] == '$') { at++; if (number == 0) throw Bad("format specifies argument 0, but arguments are numbered from 1"); position = number; }
                else { width = number; widthRead = true; }
            }
            if (!widthRead)
            {
                while (at < format.Length && format[at] == '-') { left = true; at++; }
                if (at < format.Length && format[at] == '*')
                {
                    at++; var widthPosition = next;
                    if (at < format.Length && format[at] is >= '0' and <= '9') { widthPosition = Number(); if (at == format.Length || format[at++] != '$') throw Bad("width argument position must be ended by \"$\""); }
                    var value = Argument(widthPosition);
                    var amount = value.IsNull ? 0 : value.Type.IsInteger() ? value.AsInteger() : IntegerInput.ParseText(Render(value), SqlType.I32);
                    if (amount is <= int.MinValue or > int.MaxValue) throw Errors.Overflow();
                    if (amount < 0) left = true; width = (int)Math.Abs(amount);
                }
                else if (at < format.Length && format[at] is >= '0' and <= '9') width = Number();
            }
            if (at == format.Length) throw Bad("unterminated format() type specifier"); var type = format[at++];
            if (type is not ('s' or 'I' or 'L')) throw Bad("unrecognized format() type specifier \"" + type + "\"");
            var argument = Argument(position < 0 ? next : position);
            if (type == 'I' && argument.IsNull) throw new SqlException("22004", "null values cannot be formatted as an SQL identifier");
            var rendered = argument.IsNull ? type == 'L' ? "NULL" : "" : Render(argument);
            if (!argument.IsNull) rendered = type == 'I' ? Identifier(rendered) : type == 'L' ? Literal(rendered) : rendered;
            Piece(rendered, Math.Max(0, width - rendered.EnumerateRunes().Count()), left);
        }
        meter.ScalarOutput(bytes);
        var output = new StringBuilder();
        foreach (var piece in pieces) { if (!piece.Left) output.Append(' ', piece.Padding); output.Append(piece.Text); if (piece.Left) output.Append(' ', piece.Padding); }
        return SqlValue.Text(output.ToString());
    }
    private static SqlException Bad(string detail) => new("22023", detail);
}
