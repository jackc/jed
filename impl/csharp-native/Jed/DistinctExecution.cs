using System.Runtime.CompilerServices;

namespace Jed;

internal static class DistinctExecution
{
    internal static async IAsyncEnumerable<Row> Run(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken, bool sourceMemory = false, QueryInputMemory? inputOwner = null)
    {
        if (plan.Spill is { } spill)
        {
            await foreach (var row in SpillDistinct.Run(plan, source, spill, inputOwner, meter, cancellationToken).ConfigureAwait(false)) yield return row;
            yield break;
        }
        using var inputMemory = new QueryInputMemory(meter);
        using var keptMemory = new QueryInputMemory(meter);
        using var stateMemory = new QueryInputMemory(meter);
        var touched = StorageTouchedColumns.Collect(plan);
        var seen = new HashSet<string>(StringComparer.Ordinal);
        var retained = new List<(SqlValue[] Values, SortValue[] Keys, int Ordinal)>();
        var order = plan.Order ?? [];
        long skipped = 0, produced = 0;
        if (plan.StreamDistinct && plan.Limit == 0) yield break;
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(plan.Filter, row, meter, plan.Observation?.Filter).ConfigureAwait(false)) { inputOwner?.Release(row); continue; }
            if (plan.StreamDistinct)
            {
                var projected = new SqlValue[plan.Projections.Length];
                for (var i = 0; i < projected.Length; i++) projected[i] = await Evaluate.ExprAsync(plan.Projections[i].Expression, meter, row: row).ConfigureAwait(false);
                foreach (var value in projected) GroupExecution.ChargeKey(value, meter);
                inputOwner?.Release(row);
                if (!seen.Add(GroupExecution.Identity(projected))) continue;
                if (meter.Memory.Enabled) stateMemory.ReserveBytes(MemorySize.EntryBytes(projected));
                if (skipped++ < plan.Offset) continue;
                meter.Charge(Costs.RowProduced); meter.Checkpoint(); produced++;
                yield return new Row(projected);
                if (plan.Limit is { } limit && produced >= limit) yield break;
            }
            else
            {
                var keys = new SortValue[order.Length];
                for (var k = 0; k < keys.Length; k++) keys[k] = CollationRules.Decorate(await Evaluate.ExprAsync(order[k].Expression, meter, row: row).ConfigureAwait(false), order[k].Expression, meter);
                var buffered = sourceMemory ? row : await inputMemory.TakeAsync(row, false, touched).ConfigureAwait(false);
                for (var k = 0; k < keys.Length; k++) if (order[k].Expression.Op != "column") inputMemory.ReserveValue(keys[k].Value);
                retained.Add((buffered, keys, retained.Count));
            }
        }
        if (plan.StreamDistinct) yield break;
        if (order.Length != 0) retained.Sort((left, right) =>
        {
            for (var i = 0; i < order.Length; i++)
            {
                var compare = SortValue.Compare(left.Keys[i], right.Keys[i], order[i]);
                if (compare != 0) return compare;
            }
            return left.Ordinal.CompareTo(right.Ordinal);
        });
        var unique = new List<SqlValue[]>();
        using var hostBatch = HostBatchReplay.HasEligible(plan.Projections.Select(item => item.Expression))
            ? meter.HostBatches.Begin(plan.Projections.Select(item => item.Expression), retained.Select(item => item.Values).ToArray()) : null;
        foreach (var row in retained)
        {
            var projected = new SqlValue[plan.Projections.Length];
            for (var i = 0; i < projected.Length; i++) projected[i] = await Evaluate.ExprAsync(plan.Projections[i].Expression, meter, row: row.Values).ConfigureAwait(false);
            foreach (var value in projected) GroupExecution.ChargeKey(value, meter);
            if (!seen.Add(GroupExecution.Identity(projected))) continue;
            if (meter.Memory.Enabled) stateMemory.ReserveBytes(MemorySize.EntryBytes(projected));
            keptMemory.Reserve(projected); unique.Add(projected);
        }
        inputMemory.Dispose(); inputOwner?.Dispose(); stateMemory.Dispose();
        foreach (var row in unique)
        {
            if (skipped++ < plan.Offset) { keptMemory.Release(row); continue; }
            if (plan.Limit is { } limit && produced >= limit) break;
            keptMemory.Release(row);
            meter.Charge(Costs.RowProduced); meter.Checkpoint(); produced++;
            yield return new Row(row);
        }
    }
}
