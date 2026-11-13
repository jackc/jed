using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record SpillPolicy(ScratchHost Host, long WorkMem, CatalogTable? Table)
{
    internal bool TopK(SelectPlan plan)
    {
        if (plan.Limit == 0) return true;
        if (Table is null || plan.Limit is not { } limit || limit > long.MaxValue - plan.Offset) return false;
        long bytes = 8 + 40L * Table.Columns.Length;
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
        var width = -1;
        int Compare(SqlValue[] a, SqlValue[] b)
        {
            for (var i = 0; i < slots.Length; i++)
            {
                var result = SortValue.Compare(new(a[slots[i]], null), new(b[slots[i]], null), order[i]);
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
            }
            var retained = new SqlValue[row.Length + order.Count(key => SortPlan.Column(key.Expression) is null)];
            for (var i = 0; i < row.Length; i++) retained[i] = touched.Contains(i) ? await row[i].ResolveAsync(token).ConfigureAwait(false) : SqlValue.Null(row[i].Type);
            for (var i = 0; i < order.Length; i++)
                if (SortPlan.Column(order[i].Expression) is null) retained[slots[i]] = await Evaluate.ExprAsync(order[i].Expression, meter, row: row).ConfigureAwait(false);
            inputMemory.Release(row);
            await sorter.Add(retained, token).ConfigureAwait(false);
        }
        long skipped = 0, produced = 0;
        await foreach (var row in sorter.Read(token).ConfigureAwait(false))
        {
            if (skipped++ < plan.Offset) continue;
            if (plan.Limit is { } limit && produced >= limit) break;
            produced++;
            yield return await QueryExecution.ProjectAsync(plan.Projections, row, meter, plan.ChargeRows).ConfigureAwait(false);
        }
    }
}
