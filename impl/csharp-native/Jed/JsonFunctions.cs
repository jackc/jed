using System.Text;

namespace Jed;

internal static class JsonFunctions
{
    internal static bool TryCall(string name, SqlValue[] args, Meter meter, out SqlValue result)
    {
        result = default;
        if (JsonPathFunctions.TryCall(name, args, meter, out result)) return true;
        var binary = name.StartsWith("jsonb", StringComparison.Ordinal) || name == "to_jsonb";
        var resultType = binary ? SqlType.Jsonb : SqlType.Json;
        if (name is "json_typeof" or "jsonb_typeof")
        {
            var node = JsonOps.Node(args[0], decode: false);
            result = SqlValue.Text(node.Kind switch { SqlJsonKind.Null => "null", SqlJsonKind.False or SqlJsonKind.True => "boolean", SqlJsonKind.Number => "number", SqlJsonKind.String => "string", SqlJsonKind.Array => "array", _ => "object" }); return true;
        }
        if (name is "json_array_length" or "jsonb_array_length")
        {
            var node = JsonOps.Node(args[0], decode: false); if (node.Kind != SqlJsonKind.Array) throw new SqlException("22023", "cannot get array length of a non-array");
            result = SqlValue.Integer(node.Elements.Count, SqlType.I32); return true;
        }
        if (name is "json_strip_nulls" or "jsonb_strip_nulls")
        {
            SqlJsonNode Strip(SqlJsonNode node) => node.Kind switch
            {
                SqlJsonKind.Array => SqlJsonNode.Array(node.Elements.Select(Strip)),
                SqlJsonKind.Object => new(SqlJsonKind.Object, members: node.Members.Where(member => member.Value.Kind != SqlJsonKind.Null).Select(member => new SqlJsonMember(member.Key, Strip(member.Value))).ToArray()),
                _ => node
            };
            var stripped = Strip(JsonOps.Node(args[0]));
            if (binary) result = SqlValue.Jsonb(stripped);
            else { var text = new StringBuilder(); stripped.Render(text, spaces: false); result = SqlValue.Json(text.ToString()); }
            return true;
        }
        if (name == "jsonb_pretty") { result = Pretty(args[0].AsJsonb(), meter); return true; }
        if (name is "to_json" or "to_jsonb" or "array_to_json" or "row_to_json" or "json_scalar")
        {
            meter.Charge(TemporalRender.ZoneCount(args[0]) * Costs.Timezone); meter.Guard();
            result = binary ? SqlValue.Jsonb(JsonImage.Node(args[0], meter.Temporal)) : SqlValue.Json(JsonImage.Text(args[0], meter.Temporal, args.Length > 1 && args[1].AsBoolean())); return true;
        }
        if (name == "json_serialize") { result = SqlValue.Text(args[0].ToString()); return true; }
        if (name is "json_build_array" or "jsonb_build_array" or "json_build_object" or "jsonb_build_object")
        {
            var objects = name.EndsWith("object", StringComparison.Ordinal);
            if (objects && args.Length % 2 != 0) throw new SqlException("22023", "argument list must have even number of elements");
            meter.Charge(args.Length + args.Sum(TemporalRender.ZoneCount) * Costs.Timezone); meter.Guard();
            if (!objects)
                result = binary ? SqlValue.Jsonb(SqlJsonNode.Array(args.Select(arg => JsonImage.Node(arg, meter.Temporal)))) : SqlValue.Json("[" + string.Join(", ", args.Select(arg => JsonImage.Text(arg, meter.Temporal))) + "]");
            else
            {
                var members = new List<SqlJsonMember>(); var rendered = new List<string>();
                for (var i = 0; i < args.Length; i += 2)
                {
                    var key = JsonImage.Key(args[i], meter.Temporal, binary, i + 1);
                    if (binary) members.Add(new(key, JsonImage.Node(args[i + 1], meter.Temporal)));
                    else rendered.Add(JsonImage.Quote(key) + " : " + JsonImage.Text(args[i + 1], meter.Temporal));
                }
                result = binary ? SqlValue.Jsonb(SqlJsonNode.Object(members)) : SqlValue.Json("{" + string.Join(", ", rendered) + "}");
            }
            return true;
        }
        if (name is "json_object" or "jsonb_object")
        {
            var keys = args[0].AsArray(); var pairs = new List<(SqlValue Key, SqlValue Value)>();
            if (args.Length == 2)
            {
                var values = args[1].AsArray();
                if (keys.Rank > 1 || values.Rank > 1) throw new SqlException("2202E", "must be one-dimensional array");
                if (keys.Count != values.Count) throw new SqlException("2202E", "mismatched array dimensions");
                for (var i = 0; i < keys.Count; i++) pairs.Add((keys.Values[i], values.Values[i]));
            }
            else
            {
                if (keys.Rank > 2 || keys.Rank == 2 && keys.Dimensions[1] != 2) throw new SqlException("2202E", "array must have two columns");
                if (keys.Count % 2 != 0) throw new SqlException("2202E", "array must have even number of elements");
                for (var i = 0; i < keys.Count; i += 2) pairs.Add((keys.Values[i], keys.Values[i + 1]));
            }
            meter.Charge(pairs.Count); meter.Guard();
            var members = pairs.Select(pair => new SqlJsonMember(JsonImage.Key(pair.Key, meter.Temporal, binary: false), pair.Value.IsNull ? SqlJsonNode.Null : SqlJsonNode.String(pair.Value.AsText()))).ToArray();
            if (binary) result = SqlValue.Jsonb(SqlJsonNode.Object(members));
            else { var text = new StringBuilder(); new SqlJsonNode(SqlJsonKind.Object, members: members).Render(text, spaces: true, objectColonSpaces: true); result = SqlValue.Json(text.ToString()); }
            return true;
        }
        if (name is "jsonb_set" or "jsonb_set_lax" or "jsonb_insert")
        {
            if (args[0].IsNull || args[1].IsNull || args[3].IsNull) { result = SqlValue.Null(SqlType.Jsonb); return true; }
            if (name == "jsonb_set_lax" && args[4].IsNull) throw new SqlException("22023", "invalid null_value_treatment");
            if (args[2].IsNull)
            {
                if (name != "jsonb_set_lax") { result = SqlValue.Null(SqlType.Jsonb); return true; }
                switch (args[4].AsText())
                {
                    case "return_target": result = args[0]; return true;
                    case "delete_key": result = SqlValue.Jsonb(JsonOps.Mutate(args[0].AsJsonb(), args[1].AsArray(), null, "delete", false)); return true;
                    case "raise_exception": throw new SqlException("22004", "JSON value must not be null");
                    case "use_json_null": break;
                    default: throw new SqlException("22023", "invalid null_value_treatment");
                }
            }
            result = SqlValue.Jsonb(JsonOps.Mutate(args[0].AsJsonb(), args[1].AsArray(), args[2].IsNull ? SqlJsonNode.Null : args[2].AsJsonb(), name == "jsonb_insert" ? "insert" : "set", args[3].AsBoolean())); return true;
        }
        return false;
    }
    private static SqlValue Pretty(SqlJsonNode node, Meter meter)
    {
        static long QuotedSize(string text) => TextEncoding.ByteCount(text) + 2L + text.Sum(ch => ch is '"' or '\\' or '\b' or '\f' or '\n' or '\r' or '\t' ? 1 : ch < 32 ? 5 : 0);
        long Size(SqlJsonNode value, int depth)
        {
            if (value.Kind == SqlJsonKind.String) return QuotedSize(value.Text!);
            if (JsonOps.Scalar(value)) return TextEncoding.ByteCount(value.ToString());
            var count = value.Kind == SqlJsonKind.Array ? value.Elements.Count : value.Members.Count;
            long size = 3 + depth * 4L;
            for (var i = 0; i < count; i++)
            {
                size += (depth + 1) * 4L + (i == count - 1 ? 1 : 2);
                if (value.Kind == SqlJsonKind.Array) size += Size(value.Elements[i], depth + 1);
                else size += QuotedSize(value.Members[i].Key) + 2 + Size(value.Members[i].Value, depth + 1);
            }
            return size;
        }
        var bytes = Size(node, 0); meter.Charge(node.PayloadBytes - 32); meter.Guard(); meter.Charge(bytes); meter.Guard(); meter.ScalarReserve(bytes);
        if (bytes > int.MaxValue) throw Errors.Overflow();
        var output = new StringBuilder((int)bytes);
        void Render(SqlJsonNode value, int depth)
        {
            if (JsonOps.Scalar(value)) { value.Render(output); return; }
            var array = value.Kind == SqlJsonKind.Array; output.Append(array ? '[' : '{').Append('\n'); var count = array ? value.Elements.Count : value.Members.Count;
            for (var i = 0; i < count; i++)
            {
                output.Append(' ', (depth + 1) * 4);
                if (!array) { SqlJsonNode.Quote(output, value.Members[i].Key); output.Append(": "); }
                Render(array ? value.Elements[i] : value.Members[i].Value, depth + 1);
                if (i + 1 != count) output.Append(','); output.Append('\n');
            }
            output.Append(' ', depth * 4).Append(array ? ']' : '}');
        }
        Render(node, 0); return SqlValue.Text(output.ToString());
    }
}
