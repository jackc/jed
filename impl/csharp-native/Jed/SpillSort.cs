using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record SpillPolicy(ScratchHost Host, long WorkMem, CatalogTable? Table)
{
    internal bool TopK(SelectPlan plan)
    {
        if (plan.Limit == 0) return true;
        if (Table is null || plan.Limit is not { } limit || limit > long.MaxValue - plan.Offset) return false;
        long bytes = 8 + 40L * Table.Columns.Length;
        foreach (var key in plan.Order ?? [])
        {
            if (SortPlan.Column(key.Expression) is not null) continue;
            if (key.Expression.Type.Kind is SqlKind.Decimal or SqlKind.Text or SqlKind.Bytea or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath or SqlKind.Array or SqlKind.Composite or SqlKind.Range) return false;
            bytes = Estimate.Add(bytes, 40);
        }
        var touched = StorageTouchedColumns.Collect(plan);
        foreach (var slot in touched)
        {
            if ((uint)slot >= Table.Columns.Length) return false;
            var type = Table.Columns[slot].Type;
            if (type.Type.Kind == SqlKind.Decimal)
            {
                if (type.Precision is not { } precision) return false;
                bytes = Estimate.Add(bytes, 4L * ((precision + 3) / 4));
            }
            else if (type.Type.Kind is SqlKind.Text or SqlKind.Bytea or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath or SqlKind.Array or SqlKind.Composite or SqlKind.Range) return false;
        }
        return Estimate.Multiply(bytes, limit + plan.Offset) <= WorkMem;
    }
}

internal static class SpillSort
{
    internal static async IAsyncEnumerable<Row> Run(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source,
        SpillPolicy spill, QueryInputMemory inputMemory, Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        var order = plan.Order!;
        var touched = StorageTouchedColumns.Collect(plan);
        var slots = new int[order.Length];
        var keySlots = Enumerable.Repeat(-1, order.Length).ToArray();
        var width = -1;
        int Compare(SqlValue[] a, SqlValue[] b)
        {
            for (var i = 0; i < slots.Length; i++)
            {
                var result = SortValue.Compare(new(a[slots[i]], keySlots[i] < 0 ? null : a[keySlots[i]].AsBytes()),
                    new(b[slots[i]], keySlots[i] < 0 ? null : b[keySlots[i]].AsBytes()), order[i]);
                if (result != 0) return result;
            }
            return 0;
        }
        await using var sorter = new ExternalSorter(spill.Host, spill.WorkMem, meter, Compare);
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(plan.Filter, row, meter).ConfigureAwait(false)) { inputMemory.Release(row); continue; }
            if (width < 0)
            {
                width = row.Length; var next = width;
                for (var i = 0; i < order.Length; i++) slots[i] = SortPlan.Column(order[i].Expression) ?? next++;
                for (var i = 0; i < order.Length; i++) if (order[i].Expression.Collation?.Table is not null) keySlots[i] = next++;
            }
            var retained = new SqlValue[row.Length + order.Count(key => SortPlan.Column(key.Expression) is null) + keySlots.Count(slot => slot >= 0)];
            for (var i = 0; i < row.Length; i++) retained[i] = touched.Contains(i) ? await row[i].ResolveAsync(token).ConfigureAwait(false) : SqlValue.Null(row[i].Type);
            var keyCost = meter.Accrued;
            for (var i = 0; i < order.Length; i++)
            {
                if (SortPlan.Column(order[i].Expression) is null) retained[slots[i]] = await Evaluate.ExprAsync(order[i].Expression, meter, row: row).ConfigureAwait(false);
                var key = CollationRules.Decorate(retained[slots[i]], order[i].Expression, meter);
                if (keySlots[i] >= 0) retained[keySlots[i]] = SqlValue.Bytes(key.Key ?? []);
            }
            plan.Observation?.Sort?.AttributeLocal(meter.Accrued - keyCost);
            inputMemory.Release(row);
            await sorter.Add(retained, token).ConfigureAwait(false);
        }
        long skipped = 0, produced = 0;
        var roots = plan.Projections.Select(item => item.Expression).ToArray();
        long? Remaining() => plan.Limit is { } limit ? Math.Max(0, Meter.Add(plan.Offset, limit) - skipped) : null;
        // A metered merge updates its charged run heads on each pull. Keep that
        // account schedule scalar; an unmetered merge may read the full window.
        var output = HostPullWindow.Read(sorter.Read(token), roots, meter, Costs.RowProduced, Remaining, token,
            maximum: meter.Memory.Enabled ? 1 : 64);
        await foreach (var row in output.ConfigureAwait(false))
        {
            if (skipped++ < plan.Offset) continue;
            if (plan.Limit is { } limit && produced >= limit) break;
            produced++;
            yield return await QueryExecution.ProjectAsync(plan.Projections, row, meter, plan.ChargeRows).ConfigureAwait(false);
        }
    }
}
