using System.Runtime.CompilerServices;

namespace Jed;

internal static class WindowPrefix
{
    internal static bool Eligible(SelectPlan plan, CatalogTable table, bool descending)
    {
        if (plan.Windows is null || plan.Group is not null || plan.Distinct || plan.Limit is null) return false;
        foreach (var window in plan.Windows.Plans)
        {
            if (window.Partition.Length != 0 || window.Order.Length == 0 || window.Order.Length > table.PrimaryKey.Length) return false;
            for (var i = 0; i < window.Order.Length; i++)
                if (window.Order[i].Expression.Op != "column" || window.Order[i].Expression.Slot != table.PrimaryKey[i] || window.Order[i].Descending != descending) return false;
            if (window.Name is "percent_rank" or "cume_dist" or "ntile" or "lead") return false;
            if (window.Name == "lag")
            {
                if (window.Args.Length > 1 && (window.Args[1].Op != "constant" || window.Args[1].Constant.IsNull || window.Args[1].Constant.AsInteger() < 0)) return false;
                continue;
            }
            if (window.Name is "row_number" or "rank" or "dense_rank") continue;
            var end = window.Frame?.End.Kind ?? "current";
            if (end is "following" or "unbounded_following") return false;
            if (end == "current" && window.Frame?.Mode != "rows" && window.Order.Length != table.PrimaryKey.Length) return false;
        }
        return true;
    }
    internal static async IAsyncEnumerable<SqlValue[]> Take(IAsyncEnumerable<SqlValue[]> source, Resolved? filter, long count,
        Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        if (count <= 0) yield break;
        long produced = 0;
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(filter, row, meter).ConfigureAwait(false)) continue;
            yield return row;
            if (++produced >= count) yield break;
        }
    }
}
