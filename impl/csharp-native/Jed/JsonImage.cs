using System.Globalization;
using System.Text;

namespace Jed;

internal static class JsonImage
{
    internal static SqlJsonNode Node(SqlValue value, TemporalContext context)
    {
        if (value.IsNull) return SqlJsonNode.Null;
        if (value.Type == SqlType.Jsonb) return value.AsJsonb();
        if (value.Type == SqlType.Json) return JsonInput.Parse(value.AsJsonText());
        if (value.Type == SqlType.Boolean) return SqlJsonNode.Boolean(value.AsBoolean());
        if (value.Type.IsInteger()) return SqlJsonNode.Numeric(ExactDecimal.FromInt64(value.AsInteger()));
        if (value.Type == SqlType.Decimal) return SqlJsonNode.Numeric(value.AsDecimal());
        if (value.Type.IsFloat()) return double.IsFinite(value.AsFloat()) ? SqlJsonNode.Numeric(ExactDecimal.Parse(value.ToString())) : SqlJsonNode.String(value.ToString());
        if (value.Type.Kind == SqlKind.Composite)
            return SqlJsonNode.Object(value.AsComposite().Fields.Select((field, index) => new SqlJsonMember(value.Type.Fields[index].Name, Node(field, context))));
        if (value.Type.Kind == SqlKind.Array)
        {
            var array = value.AsArray(); var offset = 0;
            SqlJsonNode Dimension(int depth)
            {
                if (array.Rank == 0) return SqlJsonNode.Array([]);
                var elements = new SqlJsonNode[array.Dimensions[depth]];
                for (var i = 0; i < elements.Length; i++) elements[i] = depth + 1 == array.Rank ? Node(array.Values[offset++], context) : Dimension(depth + 1);
                return SqlJsonNode.Array(elements);
            }
            return Dimension(0);
        }
        return SqlJsonNode.String(ScalarText(value, context));
    }
    internal static string Text(SqlValue value, TemporalContext context, bool pretty = false)
    {
        if (value.IsNull) return "null";
        if (value.Type == SqlType.Json) return value.AsJsonText();
        if (value.Type == SqlType.Jsonb) return value.AsJsonb().ToString();
        if (value.Type.IsInteger() || value.Type == SqlType.Decimal || value.Type == SqlType.Boolean || value.Type.IsFloat() && double.IsFinite(value.AsFloat())) return value.ToString();
        if (value.Type.Kind == SqlKind.Array)
        {
            var array = value.AsArray(); var offset = 0;
            string Dimension(int depth)
            {
                if (array.Rank == 0) return "[]";
                var values = new string[array.Dimensions[depth]];
                for (var i = 0; i < values.Length; i++) values[i] = depth + 1 == array.Rank ? Text(array.Values[offset++], context) : Dimension(depth + 1);
                return "[" + string.Join(pretty && depth == 0 ? ",\n " : ",", values) + "]";
            }
            return Dimension(0);
        }
        if (value.Type.Kind == SqlKind.Composite)
        {
            var record = value.AsComposite();
            return "{" + string.Join(pretty ? ",\n " : ",", record.Fields.Select((field, index) => Quote(value.Type.Fields[index].Name) + ":" + Text(field, context))) + "}";
        }
        return Quote(ScalarText(value, context));
    }
    internal static string Quote(string value) { var output = new StringBuilder(); SqlJsonNode.Quote(output, value); return output.ToString(); }
    internal static string Key(SqlValue value, TemporalContext context, bool binary, int argument = 0)
    {
        if (value.IsNull) throw new SqlException(binary ? "22023" : "22004", binary ? $"argument {argument}: key must not be null" : "null value not allowed for object key");
        if (value.Type.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Json or SqlKind.Jsonb) throw new SqlException("22023", "key value must be scalar, not array, composite, or json");
        return ScalarText(value, context);
    }
    private static string ScalarText(SqlValue value, TemporalContext context)
    {
        var text = TemporalRender.Value(value, context);
        if (value.Type.Kind is not (SqlKind.Timestamp or SqlKind.Timestamptz) || TemporalValue.IsInfinite(value.AsTimestamp())) return text;
        var separator = text.IndexOf(' '); if (separator >= 0) text = text[..separator] + "T" + text[(separator + 1)..];
        if (value.Type == SqlType.Timestamptz)
        {
            var bc = text.EndsWith(" BC", StringComparison.Ordinal); if (bc) text = text[..^3];
            var offset = context.ResolveZone().OffsetAt(value.AsTimestamp()).Seconds;
            var absolute = Math.Abs((long)offset); var suffix = (offset < 0 ? "-" : "+") + (absolute / 3600).ToString("D2", CultureInfo.InvariantCulture);
            if (absolute % 3600 == 0 && text.EndsWith(suffix, StringComparison.Ordinal)) text += ":00";
            if (bc) text += " BC";
        }
        return text;
    }
}
