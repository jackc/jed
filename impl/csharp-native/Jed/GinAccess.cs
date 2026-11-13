using System.Runtime.CompilerServices;

namespace Jed;

internal sealed record GinAccess(CatalogIndex Index, StorageTree Tree, int Column, GinQueryNode[] Arms,
    long EstimatedRows, long EstimatedEntries, long EstimatedPages, long EstimatedCost)
{
    internal static GinAccess? Select(CatalogTable table, StorageTree primary, Resolved? filter, QueryStorage context, Meter? meter = null)
    {
        return Candidates(table, primary, filter, context, meter).OrderBy(candidate => candidate.EstimatedCost)
            .FirstOrDefault(candidate => candidate.EstimatedCost <= AccessPlan.Primary(table, primary, filter).EstimatedCost);
    }
    internal static IEnumerable<GinAccess> Candidates(CatalogTable table, StorageTree primary, Resolved? filter, QueryStorage context, Meter? meter = null)
    {
        if (filter is null) yield break;
        foreach (var index in (table.Indexes ?? []).Where(index => index.Kind == 1).OrderBy(index => index.Name, Utf8NameComparer.Instance))
        {
            if (!StatementExecution.IndexDependenciesUsable(index, context.Catalog)) continue;
            if (index.Predicate is not null && !SecondaryAccess.Implies(filter, StatementExecution.ResolveIndexPredicate(table, index, context.Catalog)!)) continue;
            for (var slot = 0; slot < index.Keys.Length; slot++)
            {
                var column = index.Keys[slot].Column;
                var arms = GinQuery.Match(filter, column, table.Columns[column].Type.Type, meter); if (arms is null) continue;
                var tree = context.Database.IndexTree(index, context.Pending, context.Snapshot);
                var stats = StatisticsView.Find(context.Catalog, table, column);
                long rows = 0, entries = 0;
                foreach (var arm in arms)
                {
                    var estimate = stats is null || arm.Unbound ? (Rows: !arm.Unbound && arm.Terms.Length == 0 && arm.Children.Length == 0 ? 0 : Estimate.Fraction(table.RowCount, "matching"), Entries: 0L) : EstimateNode(arm, stats);
                    rows = Math.Min(stats?.NonnullRows ?? table.RowCount, Estimate.Add(rows, estimate.Rows));
                    entries = Estimate.Add(entries, stats is null || arm.Unbound ? estimate.Rows : estimate.Entries);
                }
                long pages = 0;
                if (entries > 0)
                {
                    var totalEntries = index.Keys.Aggregate(0L, (count, key) => Estimate.Add(count, StatisticsView.Find(context.Catalog, table, key.Column)?.TermEntries ?? 0));
                    pages = stats is null || arms.Any(arm => arm.Unbound) || totalEntries == 0 ? Estimate.Fraction(tree.NodeCount, "matching") : Estimate.Scale(tree.NodeCount, entries, totalEntries);
                    pages = Math.Clamp(pages, tree.Height, tree.NodeCount);
                }
                var cost = Estimate.Add(Estimate.Add(pages, entries), Estimate.Multiply(rows, Estimate.Add(primary.Height + 1L, PredicatePlan.Nodes(filter))));
                yield return new(index, tree, column, arms.Select(arm => arm.Tagged(index, slot)).ToArray(), rows, entries, pages, cost);
            }
        }
    }
    internal static (long Rows, long Entries) EstimateNode(GinQueryNode node, StatisticsView statistics)
    {
        if (statistics.Population == 0) return (0, 0);
        long rows = 0, entries = 0; var initialized = false;
        var terms = node.Terms.Select(term => (Term: term, Rows: statistics.TermRows(term))).ToArray();
        if (node.All) terms = terms.OrderBy(item => item.Rows).ThenBy(item => item.Term, StatisticsBytes.Instance).ToArray();
        foreach (var term in terms)
        {
            if (node.All && initialized)
            { entries = Estimate.Add(entries, rows); rows = Estimate.Scale(rows, term.Rows, statistics.Population); }
            else { entries = Estimate.Add(entries, term.Rows); rows = Math.Min(statistics.NonnullRows, Estimate.Add(rows, term.Rows)); }
            initialized = true;
        }
        foreach (var child in node.Children)
        {
            var value = EstimateNode(child, statistics); entries = Estimate.Add(entries, value.Entries);
            rows = node.All && initialized ? Estimate.Scale(rows, value.Rows, statistics.Population) : Math.Min(statistics.NonnullRows, Estimate.Add(rows, value.Rows)); initialized = true;
        }
        return (rows, entries);
    }
    internal async IAsyncEnumerable<SqlValue[]> Rows(CatalogTable table, StorageTree primary, Catalog catalog, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        await foreach (var record in Records(primary, meter, touched, token, reverse).ConfigureAwait(false))
            yield return primary.DecodeDeferredRow(table, record);
    }
    internal async IAsyncEnumerable<StorageRecord> Records(StorageTree primary, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        var candidates = new SortedSet<byte[]>(StatisticsBytes.Instance);
        foreach (var arm in Arms) candidates.UnionWith(await GinGather.Run(Tree, arm, meter, token).ConfigureAwait(false));
        foreach (var key in reverse ? candidates.Reverse() : candidates)
        {
            meter.Charge(primary.Height); meter.Checkpoint(token);
            var record = await primary.FindAsync(key, token).ConfigureAwait(false) ?? throw StorageFormat.Corrupt("GIN posting names a missing row");
            meter.Charge(primary.ValueReadUnits(record, touched)); meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
            yield return record;
        }
    }
}
