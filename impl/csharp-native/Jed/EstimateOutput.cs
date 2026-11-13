namespace Jed;

internal static partial class Estimate
{
    // Cardinality is part of a retained plan even when no EXPLAIN observer is
    // installed. Consumers such as MERGE must make the same physical choice.
    internal static long Output(SelectPlan plan, long inputRows, CatalogTable? table = null, Catalog? catalog = null)
    {
        var rows = plan.Empty ? 0 : table is null || catalog is null ? Rows(inputRows, plan.Filter) : Rows(table, catalog, plan.Filter, inputRows);
        long Groups(int keys)
        {
            if (keys == 0) return 1;
            long count = 1; for (var i = 0; i < keys; i++) count = Multiply(count, PlannerFacts.DistinctValues);
            return Math.Min(rows, count);
        }
        if (plan.Group is { } group)
        {
            rows = group.Sets is null ? Groups(group.Keys.Length) : group.Sets.Aggregate(0L, (sum, set) => Add(sum, Groups(set.Length)));
            rows = Rows(rows, group.Having);
        }
        if (plan.Distinct) rows = Groups(plan.DistinctOnCount == 0 ? plan.Projections.Length : plan.DistinctOnCount);
        rows = Math.Max(0, rows - plan.Offset);
        return plan.Limit is { } limit ? Math.Min(rows, limit) : rows;
    }
}
