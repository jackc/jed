namespace Jed;

internal static class StorageTouchedColumns
{
    internal static HashSet<int> Collect(SelectPlan plan)
    {
        var result = new HashSet<int>();
        void Visit(Resolved? node)
        {
            if (node is null) return;
            if (node.Op == "column") result.Add(node.Slot);
            if (node.Subquery is { } cell) result.UnionWith(cell.ReferencedSlots);
            foreach (var child in node.Args) Visit(child);
            foreach (var child in node.GrowthOperands ?? []) Visit(child);
        }
        Visit(plan.Filter);
        if (plan.Group is { } group)
        {
            foreach (var key in group.Keys) Visit(key);
            foreach (var aggregate in group.Aggregates) { Visit(aggregate.Argument); Visit(aggregate.Filter); foreach (var argument in aggregate.Extra ?? []) Visit(argument); foreach (var key in aggregate.Order ?? []) Visit(key.Expression); }
        }
        else if (plan.Windows is { } windows)
        {
            foreach (var window in windows.Plans)
            {
                foreach (var argument in window.Args) Visit(argument);
                foreach (var key in window.Partition) Visit(key);
                foreach (var key in window.Order) Visit(key.Expression);
                if (window.Aggregate is { } aggregate) { Visit(aggregate.Argument); Visit(aggregate.Filter); foreach (var argument in aggregate.Extra ?? []) Visit(argument); foreach (var key in aggregate.Order ?? []) Visit(key.Expression); }
            }
            foreach (var projection in plan.Projections) Visit(projection.Expression);
            foreach (var key in plan.Order ?? []) Visit(key.Expression);
        }
        else
        {
            foreach (var projection in plan.Projections) Visit(projection.Expression);
            foreach (var key in plan.Order ?? []) Visit(key.Expression);
        }
        return result;
    }
}
