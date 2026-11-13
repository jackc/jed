namespace Jed;

internal sealed record SinglePipeline(AccessPlan Access, SelectPlan Plan, bool Ordered, bool Reverse, long Cost)
{
    internal static SinglePipeline Select(CatalogTable table, StorageTree tree, SelectPlan plan, QueryStorage storage, Meter meter)
    {
        var simple = plan.Group is null && plan.Windows is null && !plan.Distinct;
        if (!simple)
        {
            var original = AccessPlan.Select(table, tree, plan.Filter, storage, meter);
            var prepared = TableAccess.Prepare(plan, table);
            return new(original, prepared.Plan with { Access = original, ScanReverse = prepared.Scan.Reverse },
                plan.Order is { Length: > 0 } && prepared.Plan.Order is not { Length: > 0 }, prepared.Scan.Reverse, original.EstimatedCost);
        }
        var candidates = AccessPlan.Inventory(table, tree, plan.Filter, storage, meter).ToList();
        var order = plan.Order ?? [];
        if (simple && table.Domain is not { Name: not ("main" or "temp") } && order.Length != 0 && plan.Limit is not null)
            foreach (var index in (table.Indexes ?? []).OrderBy(index => index.Name, Utf8NameComparer.Instance))
            {
                if (candidates.Any(candidate => candidate.Index == index) || !IndexOrder(table, index, order, storage.Catalog)) continue;
                if (index.Predicate is not null && (plan.Filter is null || !SecondaryAccess.Implies(plan.Filter, StatementExecution.ResolveIndexPredicate(table, index, storage.Catalog)!))) continue;
                var indexTree = storage.Database.IndexTree(index, storage.Pending, storage.Snapshot);
                var rows = index.Predicate is null ? table.RowCount : Estimate.Rows(table, storage.Catalog, StatementExecution.ResolveIndexPredicate(table, index, storage.Catalog));
                var access = new SecondaryAccess(index, indexTree, new(), rows, indexTree.NodeCount, 0, null);
                candidates.Add(new(AccessKind.Btree, rows, indexTree.NodeCount, 0, 0, Index: index, Btree: access));
            }
        var pk = TableAccess.Prepare(plan, table);
        var pkOrder = order.Length != 0 && pk.Plan.Order is not { Length: > 0 };
        var projection = plan.Projections.Aggregate(plan.ChargeRows ? 1L : 0, (count, item) => Estimate.Add(count, PredicatePlan.Nodes(item.Expression)));
        var filterNodes = plan.Filter is null ? 0 : PredicatePlan.Nodes(plan.Filter);
        SinglePipeline? best = null;
        foreach (var original in candidates.OrderBy(candidate => candidate.Kind).ThenBy(candidate => candidate.Index?.Name ?? "", Utf8NameComparer.Instance))
        {
            var access = original with { Ordered = original.Kind is AccessKind.Btree or AccessKind.IndexIntervals };
            var indexOrder = simple && order.Length != 0 && access.Index is { } index && (access.Btree is not null || access.IndexSet is not null) && IndexOrder(table, index, order, storage.Catalog);
            var naturallyOrdered = indexOrder || pkOrder && access.Btree is null && access.IndexSet is null;
            var streaming = simple && plan.Limit is not null && (order.Length == 0 || naturallyOrdered);
            access = access with { Streaming = streaming };
            if ((access.Btree is not null || access.IndexSet is not null) && (indexOrder || streaming)) access = access with { Ordered = true };
            var matched = Math.Min(access.EstimatedRows, Estimate.Rows(table, storage.Catalog, plan.Filter));
            var pulled = access.EstimatedRows;
            if (streaming)
            {
                var needed = plan.Limit == 0 ? 0 : Estimate.Add(plan.Offset, plan.Limit!.Value);
                // Invert the ordered, rounded selectivity program. Scaling the final
                // cardinality loses intermediate rounding and changes small-table choices.
                if (Estimate.Rows(table, storage.Catalog, plan.Filter, pulled) >= needed)
                {
                    long low = 0, high = pulled;
                    while (low < high)
                    {
                        var middle = low + (high - low) / 2;
                        if (Estimate.Rows(table, storage.Catalog, plan.Filter, middle) >= needed) high = middle;
                        else low = middle + 1;
                    }
                    pulled = low;
                }
            }
            var output = Math.Max(0, matched - plan.Offset);
            if (plan.Limit is { } limit) output = Math.Min(output, limit);
            var pages = access.EstimatedPages;
            if (streaming && access.Kind is AccessKind.Full or AccessKind.Primary or AccessKind.PrimaryIntervals && pulled != 0)
                pages = Math.Min(pages, Math.Clamp(pulled, tree.Height, Math.Max(tree.Height, tree.NodeCount)));
            if (pulled == 0) pages = 0;
            var fetch = access.Kind is AccessKind.Btree or AccessKind.Gin or AccessKind.Gist or AccessKind.IndexIntervals ? tree.Height + 1L : 1;
            var cost = Estimate.Add(Estimate.Add(pages, access.EstimatedEntries), Estimate.Multiply(pulled, Estimate.Add(fetch, filterNodes)));
            if (!naturallyOrdered) cost = Estimate.Add(cost, Estimate.Multiply(matched, order.Aggregate(0L, (count, key) => Estimate.Add(count, PredicatePlan.Nodes(key.Expression)))));
            cost = Estimate.Add(cost, Estimate.Multiply(output, projection));
            var active = naturallyOrdered ? plan with { Order = [] } : plan;
            var reverse = naturallyOrdered && !indexOrder && pk.Scan.Reverse;
            active = active with { Access = access, ScanReverse = reverse, AccessOrdering = naturallyOrdered, BeforeZeroLimit = streaming && plan.Limit == 0 ? () => access.Prime(table, tree, storage, meter, StorageTouchedColumns.Collect(plan), meter.Cancellation) : null };
            var choice = new SinglePipeline(access, active, naturallyOrdered, reverse, cost);
            if (best is null || choice.Cost < best.Cost) best = choice;
        }
        return best!;
    }
    private static bool FixedKey(SqlType type) => type.Kind is SqlKind.Boolean or SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.F32 or SqlKind.F64 or SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz or SqlKind.Uuid or SqlKind.Interval;
    internal static bool IndexOrder(CatalogTable table, CatalogIndex index, SortPlan[] order, Catalog catalog)
    {
        if (index.Kind != 0 || index.Keys.Length != order.Length || table.PrimaryKey.Length == 0 || !table.PrimaryKey.All(slot => FixedKey(table.Columns[slot].Type.Type)) ||
            !StatementExecution.IndexDependenciesUsable(index, catalog)) return false;
        for (var i = 0; i < order.Length; i++)
        {
            var key = index.Keys[i];
            if (key.Expression is not null || key.CollationSkew || SortPlan.Column(order[i].Expression) != key.Column ||
                order[i].Descending != ((key.Options & 1) != 0) || order[i].NullsFirst != ((key.Options & 2) != 0) ||
                !TableAccess.Compatible(order[i].Expression, table.Columns[key.Column])) return false;
        }
        return true;
    }
}
