using System.Runtime.CompilerServices;

namespace Jed;

internal static class PredicatePlan
{
    internal static IEnumerable<Resolved> Conjuncts(Resolved expression) => expression.Op == "and"
        ? Conjuncts(expression.Args[0]).Concat(Conjuncts(expression.Args[1])) : [expression];
    internal static Resolved? Combine(IEnumerable<Resolved> expressions)
    {
        Resolved? result = null;
        foreach (var item in expressions) result = result is null ? item : new("and", SqlType.Boolean, default, [result, item]);
        return result;
    }
    internal static Resolved? Order(Resolved? expression)
    {
        if (expression is null || expression.Op != "and") return expression;
        var original = Conjuncts(expression).ToArray();
        var sorted = original.OrderBy(Correlated).ThenBy(Nodes).ToArray();
        return original.SequenceEqual(sorted) ? expression : Combine(sorted);
    }
    private static bool Correlated(Resolved expression) => expression.Subquery?.IsCorrelated == true || expression.Args.Any(Correlated);
    internal static long Nodes(Resolved expression)
    {
        var local = expression.Op is "column" or "outer" or "constant" or "promote" ? 0L : 1;
        if (expression.Op == "in_list") local = 2L * (expression.Args.Length - 1) - 1;
        if (expression.Op == "quantified" && expression.Args.Length == 2)
        {
            var array = expression.Args[1];
            if (array.Op == "constant" && !array.ParameterUnbound && !array.Constant.IsNull) local += array.Constant.AsArray().Values.Count;
            else if (array.Op == "array" && array.Text != "nested") local += array.Args.Length;
        }
        foreach (var arg in expression.Args) { var count = Nodes(arg); local = local > long.MaxValue - count ? long.MaxValue : local + count; }
        return local;
    }
    internal static bool Safe(Resolved expression)
    {
        if (expression.Op is "<" or "<=" or ">" or ">=" && expression.Collation?.Table is not null) return false;
        if (expression.Op is "and" or "or" or "not") return expression.Args.All(Safe);
        if (expression.Op is "=" or "<>" or "!=" or "<" or "<=" or ">" or ">=" or "distinct" or "not_distinct" or "is_null" or "is_not_null" or "in" or "in_list") return expression.Args.All(Value);
        if (expression.Op is "like" or "ilike" && Value(expression.Args[0]) && expression.Args[1] is { Op: "constant" } pattern)
        {
            if (pattern.Constant.IsNull) return true;
            var text = pattern.Constant.AsText();
            for (var i = 0; i < text.Length; i++) if (text[i] == '\\' && ++i == text.Length) return false;
            return true;
        }
        return expression.Type == SqlType.Boolean && Value(expression);
    }
    private static bool Value(Resolved expression)
    {
        if (expression.Op is "column" or "constant") return true;
        if (expression.Op is "cast" or "promote")
            return expression.Target?.Precision is null && (expression.Type == expression.Args[0].Type || PlannerFacts.SafeCasts.Contains((expression.Args[0].Type, expression.Type))) && Value(expression.Args[0]);
        return expression.Function is { } function && PlannerFacts.SafeFunctions.Contains(function.Name, StringComparer.Ordinal) && expression.Args.All(Value);
    }
    internal static bool PreservesEquality(Resolved expression) =>
        !(expression.Op is "cast" or "promote" && PlannerFacts.NonEqualityCasts.Contains((expression.Args[0].Type, expression.Type))) && expression.Args.All(PreservesEquality);
    internal static HashSet<int> Slots(Resolved expression)
    {
        var slots = new HashSet<int>();
        void Visit(Resolved node) { if (node.Op == "column") slots.Add(node.Slot); foreach (var arg in node.Args) Visit(arg); }
        Visit(expression); return slots;
    }
    internal static Resolved Shift(Resolved expression, int offset) => expression with
    {
        Slot = expression.Op == "column" ? expression.Slot + offset : expression.Slot,
        Args = expression.Args.Select(arg => Shift(arg, offset)).ToArray()
    };
    internal static PreparedRelation Filter(PreparedRelation input, Resolved filter, Meter meter, CancellationToken cancellationToken)
    {
        if (input.PushFilter is { } push)
        {
            var pushed = push(filter); input = pushed.Relation;
            if (pushed.Residual is null) return input;
            filter = pushed.Residual;
        }
        var original = input; var current = filter;
        return input with { CrossChain = null, Rows = Rows(input.Rows, filter, meter, cancellationToken, input.MemoryOutput), EstimatedRows = input.EstimatedRows is { } count ? Estimate.Rows(count, filter) : null, PushFilter = next =>
            (Filter(original, Order(Combine([current, next]))!, meter, cancellationToken), null) };
    }
    internal static async IAsyncEnumerable<SqlValue[]> Rows(IAsyncEnumerable<SqlValue[]> input, Resolved filter, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken, bool memoryOutput = false)
    {
        await foreach (var row in input.WithCancellation(cancellationToken).ConfigureAwait(false))
            if (await QueryExecution.KeepsAsync(filter, row, meter).ConfigureAwait(false)) yield return row;
            else if (memoryOutput && meter.Memory.Enabled) meter.Memory.Release(MemorySize.RowBytes(row));
    }
}
