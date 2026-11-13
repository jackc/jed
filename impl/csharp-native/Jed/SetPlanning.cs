namespace Jed;

internal static class SetPlanning
{
    internal static PreparedRelation Build(PreparedRelation left, PreparedRelation right, BoundColumn[] columns,
        string operation, bool all, Meter meter, CancellationToken token)
    {
        var prepared = ExplainPlanning.Set(new(columns,
            SetExecution.Run(left.Rows, right.Rows, columns.Select(column => column.Type).ToArray(), operation, all, meter, token, left.MemoryOutput, right.MemoryOutput),
            Dependencies: (left.Dependencies ?? []).Concat(right.Dependencies ?? []).Distinct().ToArray(),
            InitializeConstants: async () =>
            {
                if (left.InitializeConstants is { } a) await a().ConfigureAwait(false);
                if (right.InitializeConstants is { } b) await b().ConfigureAwait(false);
            }, MemoryOutput: true), left, right, operation, all, meter);
        if (left.PushFilter is null || right.PushFilter is null) return prepared;
        return prepared with { PushFilter = filter =>
        {
            var a = left; var b = right; var residual = new List<Resolved>(); var changed = false;
            foreach (var term in PredicatePlan.Conjuncts(filter))
            {
                var slots = PredicatePlan.Slots(term);
                if (!PredicatePlan.Safe(term) || (!(operation == "union" && all) && !PredicatePlan.PreservesEquality(term)) || slots.Count == 0 ||
                    slots.Any(slot => !Widens(a.Columns[slot].Type, columns[slot].Type) || !Widens(b.Columns[slot].Type, columns[slot].Type)))
                { residual.Add(term); continue; }
                var pa = a.PushFilter!(Rewrite(term, a.Columns));
                var pb = b.PushFilter!(Rewrite(term, b.Columns));
                if (pa.Residual is not null || pb.Residual is not null) { residual.Add(term); continue; }
                a = pa.Relation; b = pb.Relation; changed = true;
            }
            return (changed ? Build(a, b, columns, operation, all, meter, token) : prepared, PredicatePlan.Combine(residual));
        } };
    }
    private static bool Widens(SqlType source, SqlType target)
    {
        if (source == target || source == SqlType.Unknown || source.IsInteger() && (target.IsInteger() || target == SqlType.Decimal) || source == SqlType.F32 && target == SqlType.F64) return true;
        if (source.Kind == SqlKind.Array && target.Kind == SqlKind.Array) return IntegerWidths(source.Element!, target.Element!);
        if (source.Kind == SqlKind.Composite && target.Kind == SqlKind.Composite && source.Identity == target.Identity && source.Fields.Count == target.Fields.Count)
            return source.Fields.Zip(target.Fields).All(pair => IntegerWidths(pair.First.Type, pair.Second.Type));
        return false;
    }
    private static bool IntegerWidths(SqlType a, SqlType b) => a == b || a.IsInteger() && b.IsInteger() ||
        a.Kind == SqlKind.Array && b.Kind == SqlKind.Array && IntegerWidths(a.Element!, b.Element!) ||
        a.Kind == SqlKind.Composite && b.Kind == SqlKind.Composite && a.Identity == b.Identity && a.Fields.Count == b.Fields.Count && a.Fields.Zip(b.Fields).All(pair => IntegerWidths(pair.First.Type, pair.Second.Type));
    private static Resolved Rewrite(Resolved expression, BoundColumn[] columns)
    {
        if (expression.Op == "column")
        {
            var type = columns[expression.Slot].Type; var column = expression with { Type = type };
            if (type.IsInteger() && expression.Type == SqlType.Decimal || type == SqlType.F32 && expression.Type == SqlType.F64)
                return new("cast", expression.Type, default, [column], Target: new(expression.Type), Collation: expression.Collation);
            return column;
        }
        return expression with { Args = expression.Args.Select(arg => Rewrite(arg, columns)).ToArray() };
    }
}
