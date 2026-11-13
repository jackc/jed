using System.Globalization;
using System.Text;

namespace Jed;

internal static class ExplainExpression
{
    internal static string Quote(string text) => "'" + text.Replace("\\", "\\\\", StringComparison.Ordinal).Replace("'", "''", StringComparison.Ordinal) + "'";
    internal static string Identifier(string text) => text.Length != 0 && (char.IsAsciiLetterLower(text[0]) || text[0] == '_') && text.All(c => char.IsAsciiLetterLower(c) || char.IsAsciiDigit(c) || c == '_')
        ? text : "\"" + text.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";
    internal static string Literal(SqlValue value)
    {
        if (value.IsNull) return "null";
        return value.Type.IsNumeric() || value.Type == SqlType.Boolean ? value.ToString() : Quote(value.ToString());
    }
    internal static string Print(Resolved expression)
    {
        string Arg(int index) => Print(expression.Args[index]);
        string Args() => string.Join(", ", expression.Args.Select(Print));
        string Binary(string operation) => "(" + Arg(0) + " " + operation + " " + Arg(1) + ")";
        if (expression.ParameterIndex != 0) return "$" + expression.ParameterIndex.ToString(CultureInfo.InvariantCulture);
        switch (expression.Op)
        {
            case "constant": return Literal(expression.Constant);
            case "column": return "#" + expression.Slot.ToString(CultureInfo.InvariantCulture);
            case "outer": return "outer(1," + expression.Outer!.Slot.ToString(CultureInfo.InvariantCulture) + ")";
            case "collate": return "(" + Arg(0) + " collate " + Identifier(expression.Text) + ")";
            case "cast": case "promote": case "float_widen": return "cast(" + Arg(0) + " as " + expression.Type.Name() + ")";
            case "join_source": return "join";
            case "host": return expression.Text + "(" + Args() + ")";
            case "call": return (expression.Text switch { "width_bucket_array" => "width_bucket", "substring" => "substr", "position" => "strpos", _ => expression.Text }) + "(" + Args() + ")";
            case "array": return "array[" + Args() + "]";
            case "row": return "row(" + Args() + ")";
            case "field": return "(" + Arg(0) + ")." + Identifier(expression.Args[0].Type.Fields[expression.Slot].Name);
            case "subscript":
                return Arg(0) + string.Concat(expression.Args.Skip(1).Select(bound => "[" + (bound.Op == "slice" ? string.Join(':', bound.Args.Select(Print)) : Print(bound.Args[0])) + "]"));
            case "case":
            {
                var text = new StringBuilder("(case");
                for (var i = 0; i < expression.Args.Length - 1; i += 2) text.Append(" when ").Append(Arg(i)).Append(" then ").Append(Arg(i + 1));
                return text.Append(" else ").Append(Arg(expression.Args.Length - 1)).Append(" end)").ToString();
            }
            case "coalesce": case "greatest": case "least": case "nullif": return expression.Op + "(" + Args() + ")";
            case "neg": return "(-" + Arg(0) + ")";
            case "pos": return "(+" + Arg(0) + ")";
            case "not": return "(not " + Arg(0) + ")";
            case "is_null": return "(" + Arg(0) + " is null)";
            case "is_not_null": return "(" + Arg(0) + " is not null)";
            case "is_boolean": return "(" + Arg(0) + " is " + expression.Text + ")";
            case "distinct": return Binary("is distinct from");
            case "not_distinct": return Binary("is not distinct from");
            case "extract": return "extract(" + expression.Text + " from " + Arg(0) + ")";
            case "stable_date": return "cast(" + Arg(0) + " as date)";
            case "at_time_zone": return Binary("at time zone");
            case "quantified":
            {
                var split = expression.Text.IndexOf(' ');
                return "(" + Arg(0) + " " + expression.Text[..split] + " " + expression.Text[(split + 1)..] + "(" + Arg(1) + "))";
            }
            case "in_list":
            {
                var text = "(" + Arg(0) + " = " + Arg(1) + ")";
                for (var i = 2; i < expression.Args.Length; i++) text = "(" + text + " or (" + Arg(0) + " = " + Arg(i) + "))";
                return text;
            }
            case "initplan": return Literal(expression.Subquery!.Value);
            case "json_sql": return Json(expression);
            case "is_json": return "(" + Arg(0) + (expression.Json!.Negated ? " is not json " : " is json ") + expression.Json.Kind + (expression.Json.Unique ? " with unique keys" : "") + ")";
            case "json_binary": case "json_path_binary": case "open_predicate": case "range_set": return Binary(expression.Text);
            case "similar": return Binary("similar to");
            case "+": case "-": case "*": case "/": case "%": case "^": case "||":
            case "=": case "<>": case "!=": case "<": case "<=": case ">": case ">=":
            case "and": case "or": case "like": case "ilike": case "~": case "~*": case "!~": case "!~*": return Binary(expression.Op);
            default: throw new InvalidOperationException("missing EXPLAIN expression printer: " + expression.Op);
        }
    }
    private static string Json(Resolved expression)
    {
        var spec = expression.Json!; var text = new StringBuilder(expression.Text).Append('(').Append(Print(expression.Args[0]));
        if (expression.Text is "json_value" or "json_query" or "json_exists")
        {
            text.Append(", ").Append(Print(expression.Args[1]));
            if (spec.PassingNames is { Length: > 0 } names)
            {
                text.Append(" passing ");
                for (var i = 0; i < names.Length; i++)
                {
                    if (i != 0) text.Append(", ");
                    var value = expression.Args[i + 2]; var rendered = Print(value);
                    if (value.Type == SqlType.Json) rendered = "cast(" + rendered + " as jsonb)";
                    text.Append(rendered).Append(" as ").Append(Identifier(names[i]));
                }
            }
            if (expression.Text != "json_exists") text.Append(" returning ").Append(expression.Type.Name());
            if (expression.Text == "json_query") text.Append(spec.Wrapper switch
            { "without" => " without array wrapper", "conditional" => " with conditional array wrapper", _ => " with unconditional array wrapper" }).Append(" keep quotes on scalar string");
            string Behavior(string name, int index) => name == "default" ? "default cast(" + Print(expression.Args[index]) + " as " + expression.Type.Name() + ")" : name.Replace('_', ' ');
            if (expression.Text != "json_exists") text.Append(' ').Append(Behavior(spec.Empty, spec.EmptyIndex)).Append(" on empty");
            text.Append(' ').Append(Behavior(spec.Error, spec.ErrorIndex)).Append(" on error");
        }
        else if (expression.Text == "json_serialize") text.Append(" returning ").Append(expression.Type.Name());
        else if (spec.Unique) text.Append(" with unique keys");
        return text.Append(')').ToString();
    }
}
