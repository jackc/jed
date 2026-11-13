using System.Runtime.CompilerServices;

namespace Jed;

/// <summary>Native JSON set-returning kernels; the query layer supplies relation binding.</summary>
internal static class JsonSetFunctions
{
    internal static bool TryResolve(Expression expression, out Resolved result, out BoundColumn[] columns, SqlField[]? definitions = null)
    {
        if (expression.Op == "json_table") { result = JsonTable.Resolve(expression, out columns); return true; }
        if (JsonRecord.TryResolve(expression, definitions, out result, out columns)) return true;
        result = null!; columns = []; var name = expression.Text; var binary = name.StartsWith("jsonb", StringComparison.Ordinal);
        if (name is "jsonb_path_query" or "jsonb_path_query_tz")
        {
            var call = expression with { Text = name == "jsonb_path_query" ? "jsonb_path_query_array" : "jsonb_path_query_array_tz" };
            _ = JsonPathFunctions.TryResolve(call, out var resolved);
            result = resolved with { Text = name }; columns = [new(name, SqlType.Jsonb)]; return true;
        }
        var stem = binary ? name[6..] : name.StartsWith("json_", StringComparison.Ordinal) ? name[5..] : "";
        if (stem is not ("array_elements" or "array_elements_text" or "object_keys" or "each" or "each_text")) return false;
        if (expression.Args.Length != 1) throw new SqlException("42883", "JSON set-returning function requires one argument");
        var expected = binary ? SqlType.Jsonb : SqlType.Json; var argument = Resolve.Expr(expression.Args[0], expected);
        if (argument.Type != expected) throw new SqlException("42883", "invalid JSON function argument type");
        var type = stem.EndsWith("text", StringComparison.Ordinal) || stem == "object_keys" ? SqlType.Text : expected;
        result = new("json_srf", type, default, [argument], name);
        columns = stem.StartsWith("each", StringComparison.Ordinal) ? [new("key", SqlType.Text), new("value", type)] : [new(stem == "object_keys" ? name : "value", type)]; return true;
    }
    internal static async IAsyncEnumerable<SqlValue[]> Rows(Resolved call, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        if (call.Op == "json_table")
        {
            await foreach (var row in JsonTable.Rows(call, meter, cancellationToken).ConfigureAwait(false)) yield return row;
            yield break;
        }
        var args = new SqlValue[call.Args.Length];
        for (var i = 0; i < args.Length; i++) args[i] = await Evaluate.ExprAsync(call.Args[i], meter).ConfigureAwait(false);
        if (call.Op == "json_record")
        {
            foreach (var row in JsonRecord.Rows(call, args, meter.Temporal))
            {
                meter.Checkpoint(cancellationToken); meter.Charge(Costs.GeneratedRow); meter.Guard(); yield return row;
            }
            yield break;
        }
        if (args.Any(arg => arg.IsNull)) yield break;
        var name = call.Text; var binary = name.StartsWith("jsonb", StringComparison.Ordinal);
        if (name is "jsonb_path_query" or "jsonb_path_query_tz")
        {
            var evaluator = new JsonPathRun(args[1].AsJsonpath(), args[0].AsJsonb(), args[2].AsJsonb(), meter);
            foreach (var item in evaluator.Sequence(args[3].AsBoolean()))
            {
                meter.Checkpoint(cancellationToken); meter.Charge(Costs.GeneratedRow); meter.Guard(); yield return [SqlValue.Jsonb(item)];
            }
            yield break;
        }
        var stem = binary ? name[6..] : name[5..]; var text = stem.EndsWith("text", StringComparison.Ordinal);
        var node = JsonOps.Node(args[0], decode: binary);
        var array = stem.StartsWith("array", StringComparison.Ordinal);
        if (!binary && stem != "array_elements" && JsonOps.Scalar(node)) node = JsonOps.Node(args[0]);
        if (node.Kind != (array ? SqlJsonKind.Array : SqlJsonKind.Object)) throw new SqlException("22023", "cannot deconstruct JSON value of this kind");
        if (!binary && stem != "array_elements") node = JsonOps.Node(args[0]);
        var count = array ? node.Elements.Count : node.Members.Count;
        for (var i = 0; i < count; i++)
        {
            meter.Checkpoint(cancellationToken); meter.Charge(Costs.GeneratedRow); meter.Guard();
            if (stem == "object_keys") { yield return [SqlValue.Text(node.Members[i].Key)]; continue; }
            var item = array ? node.Elements[i] : node.Members[i].Value;
            var value = text ? item.Kind == SqlJsonKind.Null ? SqlValue.Null(SqlType.Text) : SqlValue.Text(item.Kind == SqlJsonKind.String ? item.Text! : binary ? item.ToString() : item.Verbatim) : binary ? SqlValue.Jsonb(item) : SqlValue.StoredJson(item.Verbatim);
            if (binary && text && !JsonOps.Scalar(item)) Evaluate.Growth(call.Args, value, meter);
            yield return array ? [value] : [SqlValue.Text(node.Members[i].Key), value];
        }
    }
}
