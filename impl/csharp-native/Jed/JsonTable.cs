using System.Runtime.CompilerServices;

namespace Jed;

/// <summary>JSON_TABLE default-plan row expansion: parent outer joins and sibling unions.</summary>
internal static class JsonTable
{
    internal static Resolved Resolve(Expression expression, out BoundColumn[] columns)
    {
        var spec = expression.Json!; var table = spec.Table!;
        var header = JsonSql.Resolve(expression with { Op = "json_sql", Text = "json_query", Args = expression.Args.Take(spec.MainCount("json_table")).ToArray(), Json = spec with { Table = null } });
        var output = new List<BoundColumn>();
        JsonTableColumnPlan[] Level(JsonTableColumn[] source)
        {
            var result = new List<JsonTableColumnPlan>();
            foreach (var column in source)
            {
                var path = SqlJsonPath.Parse(column.Path);
                if (column.Kind == "nested") { result.Add(new("nested", -1, path, null, Level(column.Children))); continue; }
                var target = SqlTypes.ParseSpec(column.Spec.Returning!); var slot = output.Count;
                if (target.Type.Kind is SqlKind.Composite or SqlKind.Array) throw Errors.Unsupported("container JSON_TABLE column");
                output.Add(new(column.Name, target.Type));
                if (column.Kind == "ordinality") { result.Add(new("ordinality", slot, path, null, [])); continue; }
                var kind = column.Kind == "value" && target.Type.Kind is SqlKind.Json or SqlKind.Jsonb ? "query" : column.Kind;
                if (kind == "exists" && target.Type != SqlType.Boolean && !target.Type.IsInteger()) throw Errors.Unsupported("EXISTS JSON_TABLE column must be boolean or integer");
                var args = new List<Expression> { new("bound", "", [], Binding: new("column", SqlType.Jsonb, default, [], Slot: 0)), new("text", column.Path, []) };
                var behavior = column.Spec with { Returning = kind == "exists" ? "boolean" : column.Spec.Returning, EmptyIndex = -1, ErrorIndex = -1 };
                if (column.Spec.EmptyIndex >= 0) { behavior = behavior with { EmptyIndex = args.Count }; args.Add(expression.Args[column.Spec.EmptyIndex]); }
                if (column.Spec.ErrorIndex >= 0) { behavior = behavior with { ErrorIndex = args.Count }; args.Add(expression.Args[column.Spec.ErrorIndex]); }
                var value = JsonSql.Resolve(new("json_sql", "json_" + kind, args.ToArray(), Json: behavior));
                result.Add(new(kind, slot, path, value, []));
            }
            return result.ToArray();
        }
        var plans = Level(table.Columns); columns = output.ToArray();
        return new("json_table", SqlType.Record(output.Select(column => new SqlField(column.Name, column.Type))), default, header.Args, "json_table", Json: spec with { Table = table with { Plan = new(plans, output.Select(column => column.Type).ToArray()) } });
    }
    internal static async IAsyncEnumerable<SqlValue[]> Rows(Resolved call, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var spec = call.Json!; var plan = spec.Table!.Plan!; var bindings = new List<JsonPathBinding>();
        // SQL/JSON TABLE evaluates every PASSING conversion before the context.
        for (var i = 2; i < call.Args.Length; i++)
        {
            var value = await Evaluate.ExprAsync(call.Args[i], meter).ConfigureAwait(false);
            var converted = JsonSql.Passing(value, spec.PassingFormats![i - 2], meter); var name = spec.PassingNames![i - 2];
            if (!bindings.Any(binding => binding.Name == name)) bindings.Add(new(name, converted, i - 1));
        }
        var input = await Evaluate.ExprAsync(call.Args[0], meter).ConfigureAwait(false);
        if (input.IsNull) yield break;
        var document = input.Type == SqlType.Jsonb ? input.AsJsonb() : JsonInput.Parse(input.ToString());
        var path = call.Args[1].Constant.AsJsonpath(); var bindingArray = bindings.ToArray(); var count = call.Args.Length - 2;
        foreach (var row in Expand(document, path, plan.Columns, plan.Types, bindingArray, count, meter, cancellationToken))
        {
            meter.Checkpoint(cancellationToken); meter.Charge(Costs.GeneratedRow); meter.Guard(); yield return row;
        }
    }
    private static IEnumerable<SqlValue[]> Expand(SqlJsonNode document, SqlJsonPath path, JsonTableColumnPlan[] columns, SqlType[] types, JsonPathBinding[] bindings, int baseCount, Meter meter, CancellationToken cancellationToken)
    {
        var ordinal = 0L;
        foreach (var item in new JsonPathRun(path, document, SqlJsonNode.Object([]), meter, baseCount, bindings).Sequence())
        {
            meter.Checkpoint(cancellationToken); ordinal++;
            var row = types.Select(SqlValue.Null).ToArray();
            foreach (var column in columns)
            {
                if (column.Kind == "nested") continue;
                if (column.Kind == "ordinality") { row[column.Slot] = SqlValue.Integer(ordinal); continue; }
                var value = JsonSql.Column(column.Value!, item, column.Path, bindings, baseCount, meter);
                row[column.Slot] = column.Kind == "exists" && types[column.Slot] != SqlType.Boolean
                    ? value.IsNull ? SqlValue.Null(types[column.Slot]) : SqlValue.Integer(value.AsBoolean() ? 1 : 0, types[column.Slot]) : value;
            }
            var nested = false;
            foreach (var column in columns)
            {
                if (column.Kind != "nested") continue; nested = true; var any = false;
                foreach (var child in Expand(item, column.Path, column.Children, types, bindings, baseCount, meter, cancellationToken))
                {
                    any = true; var merged = row.ToArray(); Copy(column.Children, child, merged); yield return merged;
                }
                if (!any) yield return row.ToArray();
            }
            if (!nested) yield return row;
        }
    }
    private static void Copy(JsonTableColumnPlan[] columns, SqlValue[] source, SqlValue[] target)
    {
        foreach (var column in columns) if (column.Kind == "nested") Copy(column.Children, source, target); else target[column.Slot] = source[column.Slot];
    }
}
