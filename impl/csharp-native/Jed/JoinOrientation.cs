using System.Runtime.CompilerServices;

namespace Jed;

// Physical input order is independent of the slots bound by the SQL resolver.
internal static class JoinOrientation
{
    internal static int Slot(int slot, int left, int right) => slot < left ? right + slot : slot - left;
    internal static Resolved? Expression(Resolved? value, int left, int right) => value is null ? null : value with
    {
        Slot = value.Op == "column" ? Slot(value.Slot, left, right) : value.Slot,
        Args = value.Args.Select(arg => Expression(arg, left, right)!).ToArray()
    };
    internal static long Cost(PreparedRelation outer, PreparedRelation inner, Resolved? authored, Resolved? on, Resolved? where, string kind, Meter meter)
    {
        var a = outer.BaseAccess!; var b = inner.BaseAccess!;
        var outerRows = outer.EstimatedRows ?? PlannerFacts.SrfRows;
        var innerRows = inner.EstimatedRows ?? PlannerFacts.SrfRows;
        var outerCost = AccessPlan.Select(a.Table, a.Tree, a.Filter, a.Storage, meter).EstimatedCost;
        var innerCost = AccessPlan.Select(b.Table, b.Tree, b.Filter, b.Storage, meter).EstimatedCost;
        var residual = PredicatePlan.Combine(new[] { on, where }.OfType<Resolved>());
        var filtered = Estimate.Multiply(Estimate.JoinRows(outer, inner, on), where is null ? 0 : PredicatePlan.Nodes(where));
        var work = Estimate.Add(innerCost, Estimate.Multiply(Estimate.Multiply(outerRows, innerRows), on is null ? 0 : PredicatePlan.Nodes(on)));
        work = Estimate.Add(work, filtered);
        var hash = HashJoinPlan.Select(authored, outer.Columns.Length, kind, outerRows, innerRows, on);
        if (hash is not null) work = Math.Min(work, Estimate.Add(filtered, Estimate.Add(innerCost, HashJoinPlan.Work(outerRows, innerRows, hash.Types, on))));
        var inl = IndexNestedLoop.Select(inner, outer.Columns.Length, kind, residual);
        if (inl is not null) work = Math.Min(work, Estimate.Add(inl.EstimateWork(outerRows, on, meter),
            Estimate.Multiply(inl.EstimateRows(outerRows, on, meter), where is null ? 0 : PredicatePlan.Nodes(where))));
        return Estimate.Add(outerCost, work);
    }
    internal static PreparedRelation Restore(PreparedRelation physical, BoundColumn[] columns, int left, int right, CancellationToken token)
    {
        return physical with
        {
            Columns = columns, Rows = Rows(physical.Rows, left, right, token),
            WithTouched = physical.WithTouched is null ? null : mask => Restore(physical.WithTouched(mask.Select(slot => Slot(slot, left, right)).ToHashSet()), columns, left, right, token),
            PushFilter = physical.PushFilter is null ? null : filter =>
            {
                var pushed = physical.PushFilter(Expression(filter, left, right)!);
                return (Restore(pushed.Relation, columns, left, right, token), Expression(pushed.Residual, right, left));
            },
            Bounded = physical.Bounded is null ? null : (order, count, filter) =>
            {
                var bounded = physical.Bounded(order.Select(key => key with { Expression = Expression(key.Expression, left, right)! }).ToArray(), count, Expression(filter, left, right));
                return bounded is null ? null : Restore(bounded, columns, left, right, token);
            }
        };
    }
    private static async IAsyncEnumerable<SqlValue[]> Rows(IAsyncEnumerable<SqlValue[]> source, int left, int right,
        [EnumeratorCancellation] CancellationToken token)
    {
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        {
            var result = new SqlValue[row.Length];
            Array.Copy(row, right, result, 0, left); Array.Copy(row, 0, result, left, right);
            yield return result;
        }
    }
}
