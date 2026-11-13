namespace Jed;

internal static class JsonRecord
{
    internal static bool TryResolve(Expression expression, SqlField[]? definitions, out Resolved result, out BoundColumn[] columns)
    {
        result = null!; columns = []; var name = expression.Text; var binary = name.StartsWith("jsonb_", StringComparison.Ordinal);
        var stem = binary ? name[6..] : name.StartsWith("json_", StringComparison.Ordinal) ? name[5..] : "";
        if (stem is not ("to_record" or "to_recordset" or "populate_record" or "populate_recordset")) return false;
        var populate = stem.StartsWith("populate", StringComparison.Ordinal); var type = binary ? SqlType.Jsonb : SqlType.Json;
        if (expression.Args.Length != (populate ? 2 : 1)) throw new SqlException("42883", "invalid record function arguments");
        SqlType recordType; var args = new List<Resolved>();
        if (populate)
        {
            if (definitions is not null) throw Errors.Syntax("column definition list is redundant for a named composite result");
            var basis = Resolve.Expr(expression.Args[0]);
            if (basis.Type.Kind != SqlKind.Composite) throw Errors.Type("first argument must have a composite type");
            if (basis.Type.Identity == "record") throw Errors.Unsupported("anonymous record base");
            recordType = basis.Type; args.Add(basis);
        }
        else
        {
            if (definitions is null) throw Errors.Syntax("record-returning function requires a column definition list");
            if (definitions.Any(field => field.Type.Kind is SqlKind.Array or SqlKind.Composite)) throw Errors.Unsupported("container column in JSON record definition");
            recordType = SqlType.Record(definitions);
        }
        var input = Resolve.Expr(expression.Args[^1], type); if (input.Type != type) throw new SqlException("42883", "invalid JSON record input type"); args.Add(input);
        result = new("json_record", recordType, default, args.ToArray(), name);
        columns = recordType.Fields.Select(field => new BoundColumn(field.Name, field.Type)).ToArray(); return true;
    }
    internal static IEnumerable<SqlValue[]> Rows(Resolved call, SqlValue[] args, TemporalContext context)
    {
        var input = args[^1]; var basis = args.Length == 2 ? args[0] : SqlValue.Null(call.Type); var set = call.Text.EndsWith("recordset", StringComparison.Ordinal);
        if (input.IsNull)
        {
            if (!set) yield return basis.IsNull ? call.Type.Fields.Select(field => SqlValue.Null(field.Type)).ToArray() : basis.AsComposite().Fields.ToArray();
            yield break;
        }
        var raw = input.Type == SqlType.Json; var node = JsonOps.Node(input, decode: !raw);
        if (raw && JsonOps.Scalar(node)) node = JsonInput.Parse(node.Verbatim, canonical: false);
        if (!set)
        {
            if (raw && node.Kind == SqlJsonKind.Object) node = JsonInput.Parse(node.Verbatim, canonical: false);
            yield return Fields(node, call.Type, basis, context); yield break;
        }
        if (node.Kind != SqlJsonKind.Array) throw new SqlException("22023", "recordset requires an array of objects");
        foreach (var row in node.Elements)
        {
            var decoded = raw && row.Kind != SqlJsonKind.Array ? JsonInput.Parse(row.Verbatim, canonical: false) : row;
            yield return Fields(decoded, call.Type, basis, context);
        }
    }
    private static SqlValue[] Fields(SqlJsonNode node, SqlType type, SqlValue basis, TemporalContext context)
    {
        if (node.Kind != SqlJsonKind.Object) throw new SqlException("22023", "record requires an object");
        var members = new Dictionary<string, SqlJsonNode>(StringComparer.Ordinal); foreach (var member in node.Members) members[member.Key] = member.Value;
        var values = basis.IsNull ? type.Fields.Select(field => SqlValue.Null(field.Type)).ToArray() : basis.AsComposite().Fields.ToArray();
        for (var i = 0; i < values.Length; i++)
        {
            var field = type.Fields[i]; if (!members.TryGetValue(field.Name, out var member)) continue;
            values[i] = Resolve.ApplyModifier(Convert(member, field.Type, context, values[i]), new(field.Type, field.Precision, field.Scale, field.Length, field.IntervalFields));
            if (field.NotNull && values[i].IsNull) throw new SqlException("23502", "null composite field");
        }
        return values;
    }
    internal static SqlValue Convert(SqlJsonNode node, SqlType type, TemporalContext context, SqlValue basis = default)
    {
        if (node.Kind == SqlJsonKind.Null) return SqlValue.Null(type);
        if (type == SqlType.Jsonb) return SqlValue.Jsonb(node.Source is null ? node : JsonInput.Parse(node.Verbatim));
        if (type == SqlType.Json) return SqlValue.Json(node.Verbatim);
        if (type == SqlType.Decimal && node.Kind == SqlJsonKind.Number) return SqlValue.Decimal(node.Number!);
        if (type.Kind == SqlKind.Composite && node.Kind == SqlJsonKind.Object) return SqlValue.Composite(new(type, Fields(node, type, basis, context)));
        if (type.Kind == SqlKind.Array && node.Kind == SqlJsonKind.Array)
        {
            var dimensions = new List<int>(); var flat = new List<SqlValue>();
            void Flatten(SqlJsonNode value, int depth)
            {
                if (value.Kind != SqlJsonKind.Array) { if (depth != dimensions.Count) throw new SqlException("2202E", "multidimensional arrays must have matching dimensions"); flat.Add(Convert(value, type.Element!, context)); return; }
                if (depth == dimensions.Count) dimensions.Add(value.Elements.Count); else if (dimensions[depth] != value.Elements.Count) throw new SqlException("2202E", "multidimensional arrays must have matching dimensions");
                foreach (var child in value.Elements) Flatten(child, depth + 1);
            }
            Flatten(node, 0); return SqlValue.Array(new(type.Element!, flat, flat.Count == 0 ? [] : dimensions));
        }
        var text = node.Kind == SqlJsonKind.String ? node.Text! : node.ToString();
        return TemporalText.Parse(text, type, context);
    }
}
