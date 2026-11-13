using System.Runtime.CompilerServices;

namespace Jed;

internal static class OpenSetFunctions
{
    internal static Resolved Resolve(Expression expression)
    {
        if (expression.Args.Length != 1) throw new SqlException(SqlStates.UndefinedFunction, "unnest requires one array");
        var array = Jed.Resolve.Expr(expression.Args[0]);
        if (array.Type.Kind != SqlKind.Array) throw new SqlException(SqlStates.UndefinedFunction, "unnest requires one array");
        return array;
    }
    internal static SqlField[]? Columns(Resolved array, SqlField[]? definitions)
    {
        var element = array.Type.Element!;
        var record = element.Kind == SqlKind.Composite && element.Identity == "record";
        if (record != (definitions is not null)) throw Errors.Syntax(record ? "record unnest requires a column definition list" : "column definition list is redundant for this unnest");
        if (definitions is null) return null;
        if (element.Fields.Count != definitions.Length || element.Fields.Where((field, index) => !FieldMatches(field.Type, definitions[index].Type)).Any())
            throw Errors.Type("unnest record shape does not match its column definition list");
        return definitions;
    }
    private static bool FieldMatches(SqlType source, SqlType target) => source == target || source.IsInteger() && target.IsInteger() ||
        source.Kind == SqlKind.Array && target.Kind == SqlKind.Array && FieldMatches(source.Element!, target.Element!);

    internal static async IAsyncEnumerable<SqlValue[]> Rows(Resolved array, Meter meter, [EnumeratorCancellation] CancellationToken cancellation, SqlField[]? definitions = null)
    {
        var value = await Evaluate.ExprAsync(array, meter).ConfigureAwait(false); if (value.IsNull) yield break;
        foreach (var element in value.AsArray().Values)
        {
            meter.Charge(Costs.GeneratedRow); meter.Checkpoint(cancellation);
            if (definitions is null) { yield return [element]; continue; }
            if (element.IsNull) { yield return definitions.Select(field => SqlValue.Null(field.Type)).ToArray(); continue; }
            var fields = element.AsComposite().Fields;
            yield return definitions.Select((field, index) => Jed.Resolve.ApplyModifier(TemporalText.Cast(fields[index], field.Type, meter), new(field.Type, field.Precision, field.Scale, field.Length, field.IntervalFields))).ToArray();
        }
    }
}
