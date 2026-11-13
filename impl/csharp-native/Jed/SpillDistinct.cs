using System.Runtime.CompilerServices;

namespace Jed;

// Deduplication keeps the first occurrence in evaluation order. Input, key
// state and output have separate bounded owners so replay never retains a
// whole partition or an unbounded list of surviving rows.
internal static class SpillDistinct
{
    internal static async IAsyncEnumerable<Row> Run(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source,
        SpillPolicy spill, QueryInputMemory? inputOwner, Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        await using var seen = new SpillKeyState(spill.Host, spill.WorkMem, meter);
        long skipped = 0, produced = 0;
        async ValueTask<SqlValue[]?> Project(SqlValue[] row)
        {
            var values = new SqlValue[plan.Projections.Length];
            for (var i = 0; i < values.Length; i++) values[i] = await Evaluate.ExprAsync(plan.Projections[i].Expression, meter, row: row).ConfigureAwait(false);
            foreach (var value in values) GroupExecution.ChargeKey(value, meter);
            var key = Convert.FromHexString(GroupExecution.Identity(values));
            if (await seen.Get(key, token).ConfigureAwait(false) is not null) return null;
            await seen.Put(key, [], MemorySize.EntryBytes(values), token).ConfigureAwait(false);
            return values;
        }
        if (plan.StreamDistinct)
        {
            if (plan.Limit == 0) yield break;
            await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
            {
                if (!await QueryExecution.KeepsAsync(plan.Filter, row, meter, plan.Observation?.Filter).ConfigureAwait(false)) { inputOwner?.Release(row); continue; }
                var projected = await Project(row).ConfigureAwait(false);
                inputOwner?.Release(row);
                if (projected is null || skipped++ < plan.Offset) continue;
                meter.Charge(Costs.RowProduced); meter.Checkpoint(); produced++;
                yield return new Row(projected);
                if (plan.Limit is { } limit && produced >= limit) yield break;
            }
            yield break;
        }

        var order = plan.Order ?? [];
        var touched = StorageTouchedColumns.Collect(plan);
        var slots = new int[order.Length];
        var keySlots = new int[order.Length];
        var width = -1;
        int Compare(SqlValue[] a, SqlValue[] b)
        {
            for (var i = 0; i < order.Length; i++)
            {
                var leftKey = keySlots[i] < 0 || a[keySlots[i]].IsNull ? null : a[keySlots[i]].AsBytes();
                var rightKey = keySlots[i] < 0 || b[keySlots[i]].IsNull ? null : b[keySlots[i]].AsBytes();
                var result = SortValue.Compare(new(a[slots[i]], leftKey), new(b[slots[i]], rightKey), order[i]);
                if (result != 0) return result;
            }
            return 0;
        }
        await using var inputs = new SpillRowSpool(spill.Host, spill.WorkMem, meter);
        await using var sorter = new ExternalSorter(spill.Host, spill.WorkMem, meter, Compare);
        await using var output = new SpillRowSpool(spill.Host, spill.WorkMem, meter);
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(plan.Filter, row, meter, plan.Observation?.Filter).ConfigureAwait(false)) { inputOwner?.Release(row); continue; }
            if (width < 0)
            {
                width = row.Length;
                for (var i = 0; i < order.Length; i++)
                {
                    slots[i] = SortPlan.Column(order[i].Expression) ?? width++;
                    keySlots[i] = order[i].Expression.Collation?.Table is null ? -1 : width++;
                }
            }
            // Evaluate ORDER expressions before copying deferred source values,
            // matching the resident operator's observable evaluation order.
            var keys = new SortValue[order.Length];
            for (var i = 0; i < order.Length; i++) keys[i] = CollationRules.Decorate(await Evaluate.ExprAsync(order[i].Expression, meter, row: row).ConfigureAwait(false), order[i].Expression, meter);
            var retained = new SqlValue[width];
            for (var i = 0; i < row.Length; i++) retained[i] = touched.Contains(i) ? await row[i].ResolveAsync(token).ConfigureAwait(false) : SqlValue.Null(row[i].Type);
            for (var i = 0; i < order.Length; i++)
            {
                retained[slots[i]] = keys[i].Value;
                if (keySlots[i] >= 0) retained[keySlots[i]] = keys[i].Key is { } bytes ? SqlValue.OwnedBytes(bytes) : SqlValue.Null(SqlType.Bytea);
            }
            inputOwner?.Release(row);
            if (order.Length == 0) await inputs.Append(retained, token).ConfigureAwait(false);
            else await sorter.Add(retained, token).ConfigureAwait(false);
        }
        var ordered = order.Length == 0 ? inputs.Read(token) : sorter.Read(token);
        await foreach (var row in ordered.ConfigureAwait(false))
            if (await Project(row).ConfigureAwait(false) is { } projected) await output.Append(projected, token).ConfigureAwait(false);
        await inputs.DisposeAsync().ConfigureAwait(false);
        await sorter.DisposeAsync().ConfigureAwait(false);
        await seen.DisposeAsync().ConfigureAwait(false);
        inputOwner?.Dispose();
        await foreach (var row in output.Read(token).ConfigureAwait(false))
        {
            if (skipped++ < plan.Offset) continue;
            if (plan.Limit is { } limit && produced >= limit) break;
            meter.Charge(Costs.RowProduced); meter.Checkpoint(); produced++;
            yield return new Row(row);
        }
    }
}
