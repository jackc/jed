using System.Runtime.CompilerServices;

namespace Jed;

internal static class DistinctExecution
{
    internal static async IAsyncEnumerable<Row> Run(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var seen = new HashSet<string>(StringComparer.Ordinal);
        var retained = new List<(SqlValue[] Values, int Ordinal)>();
        long skipped = 0, produced = 0;
        if (plan.StreamDistinct && plan.Limit == 0) yield break;
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(plan.Filter, row, meter).ConfigureAwait(false)) continue;
            var projected = new SqlValue[plan.Projections.Length];
            for (var i = 0; i < projected.Length; i++) projected[i] = await Evaluate.ExprAsync(plan.Projections[i].Expression, meter, row: row).ConfigureAwait(false);
            foreach (var value in projected) GroupExecution.ChargeKey(value, meter);
            if (!seen.Add(GroupExecution.Identity(projected))) continue;
            if (plan.StreamDistinct)
            {
                if (skipped++ < plan.Offset) continue;
                meter.Charge(Costs.RowProduced); meter.Checkpoint(); produced++;
                yield return new Row(projected);
                if (plan.Limit is { } limit && produced >= limit) yield break;
            }
            else retained.Add((projected, retained.Count));
        }
        if (plan.StreamDistinct) yield break;
        var order = plan.Order ?? [];
        var keySlots = order.Select(key => Array.FindIndex(plan.Projections, p => Resolve.Equivalent(p.Expression, key.Expression))).ToArray();
        foreach (var row in retained)
            foreach (var slot in keySlots)
            {
                var work = QueryExecution.KeyBytes(row.Values[slot]);
                if (work != 0) { meter.Charge(work * Costs.SortKey); meter.Guard(); }
            }
        if (order.Length != 0) retained.Sort((left, right) =>
        {
            for (var i = 0; i < order.Length; i++)
            {
                var a = left.Values[keySlots[i]]; var b = right.Values[keySlots[i]];
                var compare = a.IsNull || b.IsNull ? a.IsNull == b.IsNull ? 0 : a.IsNull == order[i].NullsFirst ? -1 : 1 :
                    order[i].Descending ? Evaluate.Compare(b, a) : Evaluate.Compare(a, b);
                if (compare != 0) return compare;
            }
            return left.Ordinal.CompareTo(right.Ordinal);
        });
        foreach (var row in retained)
        {
            if (skipped++ < plan.Offset) continue;
            if (plan.Limit is { } limit && produced >= limit) break;
            meter.Charge(Costs.RowProduced); meter.Checkpoint(); produced++;
            yield return new Row(row.Values);
        }
    }
}
