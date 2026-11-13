using System.Runtime.CompilerServices;

namespace Jed;

internal enum AccessKind { Primary, Btree, Gist, Gin, PrimaryIntervals, IndexIntervals, Full }

// Retained physical facts shared by execution, join selection and EXPLAIN.
// Candidate estimates never inspect table values or charge the runtime meter.
internal sealed record AccessPlan(AccessKind Kind, long EstimatedRows, long EstimatedPages, long EstimatedEntries,
    long EstimatedCost, Resolved? Predicate = null, CatalogIndex? Index = null, TableScan? Scan = null,
    TableScan[]? Ranges = null, SecondaryAccess? Btree = null, GistAccess? Gist = null, GinAccess? Gin = null, bool Ordered = false, bool Streaming = false, IndexSetAccess? IndexSet = null)
{
    internal static AccessPlan Primary(CatalogTable table, StorageTree tree, Resolved? filter, Catalog? catalog = null)
    {
        var scan = TableAccess.Bounds(filter, table); var facts = TableAccess.Predicate(filter, table);
        var bounded = facts.Predicate is not null || scan.Empty || scan.Lower is not null || scan.Upper is not null;
        var rows = scan.Empty ? 0 : catalog is null ? Estimate.Rows(table.RowCount, facts.Predicate) : Estimate.Rows(table, catalog, facts.Predicate);
        if (facts.Unique) rows = Math.Min(1, rows);
        var pages = rows == 0 ? 0 : facts.Unique ? tree.Height : Math.Clamp(catalog is null ? Estimate.Rows(tree.NodeCount, facts.Predicate) : Estimate.Rows(table, catalog, facts.Predicate, tree.NodeCount), tree.Height, tree.NodeCount);
        return new(bounded ? AccessKind.Primary : AccessKind.Full, rows, pages, 0,
            ScanCost(rows, pages, filter), facts.Predicate, Scan: scan);
    }
    internal static long ScanCost(long rows, long pages, Resolved? filter) =>
        Estimate.Add(pages, Estimate.Multiply(rows, Estimate.Add(1, filter is null ? 0 : PredicatePlan.Nodes(filter))));

    internal static AccessPlan Select(CatalogTable table, StorageTree tree, Resolved? filter, QueryStorage storage, Meter meter)
    {
        var selected = Inventory(table, tree, filter, storage, meter).OrderBy(candidate => candidate.EstimatedCost).ThenBy(candidate => candidate.Kind).ThenBy(candidate => candidate.Index?.Name ?? "", Utf8NameComparer.Instance).First();
        return selected with { Ordered = selected.Kind is AccessKind.Btree or AccessKind.IndexIntervals };
    }

    internal static AccessPlan Rebind(AccessPlan selected, CatalogTable table, StorageTree tree, Resolved? filter, QueryStorage storage, Meter meter)
    {
        var rebound = Inventory(table, tree, filter, storage, meter).FirstOrDefault(candidate => candidate.Kind == selected.Kind && candidate.Index?.Name == selected.Index?.Name);
        if (rebound is not null) return rebound with { Ordered = selected.Ordered, Streaming = selected.Streaming };
        // A promoted fractional range endpoint drops its half-bound. Keep the
        // chosen physical tree even when the remaining range spans that tree.
        if (selected.Btree is { } btree)
        {
            var keys = StatementExecution.ResolveIndexKeys(table, btree.Index, storage.Catalog);
            var scan = filter is null ? new TableScan() : SecondaryAccess.Bounds(filter, keys, btree.Index.Keys)?.Scan ?? new();
            return selected with { Scan = scan, Btree = btree with { Scan = scan } };
        }
        if (selected.Kind is AccessKind.Primary or AccessKind.Full)
            return selected with { Scan = TableAccess.Bounds(filter, table) };
        throw new InvalidOperationException("selected access path disappeared during value binding");
    }

    internal static IEnumerable<AccessPlan> Inventory(CatalogTable table, StorageTree tree, Resolved? filter, QueryStorage storage, Meter meter)
    {
        if (table.Domain is { Name: not ("main" or "temp") })
        {
            yield return new(AccessKind.Full, table.RowCount, tree.NodeCount, 0, ScanCost(table.RowCount, tree.NodeCount, filter), Scan: new());
            yield break;
        }
        var primary = Primary(table, tree, filter, storage.Catalog);
        if (primary.Kind == AccessKind.Primary) yield return primary;
        foreach (var access in SecondaryAccess.Candidates(table, tree, filter, storage))
            yield return new(AccessKind.Btree, access.EstimatedRows, access.EstimatedPages, 0, access.EstimatedCost,
                access.Predicate, access.Index, access.Scan, Btree: access);
        foreach (var access in GistAccess.Candidates(table, filter, storage, meter))
        {
            var unknown = access.Predicate.Args.Any(arg => arg.ParameterUnbound);
            var rows = !unknown && (access.Query.IsNull || access.Strategy == 0 && access.Query.Type.Kind == SqlKind.Range && access.Query.AsRange().IsEmpty)
                ? 0 : access.Strategy != 1 ? Estimate.Fraction(table.RowCount, "matching") : Estimate.Rows(table, storage.Catalog, access.Predicate);
            var root = access.Index.Gist?.Root;
            var shape = root is null ? storage.Database.IndexTree(access.Index, storage.Pending, storage.Snapshot) : null;
            var height = root is null ? shape!.Height : Height(root);
            var nodeCount = root?.Count ?? shape!.NodeCount;
            var pages = rows == 0 ? 0 : Math.Clamp(access.Strategy != 1 ? Estimate.Fraction(nodeCount, "matching") : Estimate.Rows(table, storage.Catalog, access.Predicate, nodeCount), height, nodeCount);
            var descents = rows == 0 ? 0 : unknown || root is null ? Math.Max(0, height - 1) : Descent(root, access);
            var cost = Estimate.Add(Estimate.Add(pages, descents), Estimate.Multiply(rows, Estimate.Add(tree.Height + 1L, filter is null ? 0 : PredicatePlan.Nodes(filter))));
            yield return new(AccessKind.Gist, rows, pages, descents, cost, access.Predicate, access.Index, Gist: access);
        }
        foreach (var access in GinAccess.Candidates(table, tree, filter, storage, meter))
            yield return new(AccessKind.Gin, access.EstimatedRows, access.EstimatedPages, access.EstimatedEntries,
                access.EstimatedCost, filter, access.Index, Gin: access);
        if (PointAccess.Ranges(filter, table) is { } ranges)
        {
            long rows = 0, pages = 0;
            foreach (var range in ranges)
            {
                var point = KeyIntervals.Point(range);
                var n = range.Empty ? 0 : point ? Math.Min(1, table.RowCount) : Estimate.Fraction(table.RowCount, "paired_range");
                rows = Math.Min(table.RowCount, Estimate.Add(rows, n));
                pages = Estimate.Add(pages, n == 0 ? 0 : point ? tree.Height : Math.Clamp(Estimate.Fraction(tree.NodeCount, "paired_range"), tree.Height, tree.NodeCount));
            }
            var keys = table.PrimaryKey.Select(slot => new Resolved("column", table.Columns[slot].Type.Type, default, [], Slot: slot,
                Collation: CollationRules.Collatable(table.Columns[slot].Type.Type) ? new(table.Columns[slot].Collation ?? "C", Table: table.Columns[slot].CollationData) : null)).ToArray();
            rows = KeySetEstimate.Rows(table, storage.Catalog, filter!, keys, true, table.RowCount);
            pages = rows == 0 ? 0 : Math.Clamp(KeySetEstimate.Rows(table, storage.Catalog, filter!, keys, true, tree.NodeCount), tree.Height, tree.NodeCount);
            yield return new(AccessKind.PrimaryIntervals, rows, pages, 0, ScanCost(rows, pages, filter), filter, Ranges: ranges);
        }
        foreach (var set in IndexSetAccess.Candidates(table, tree, filter, storage))
            yield return new(AccessKind.IndexIntervals, set.EstimatedRows, set.EstimatedPages, 0, set.EstimatedCost,
                set.Predicate, set.Index, Ranges: set.Ranges, IndexSet: set);
        foreach (var generic in KeySetShape.UnknownCandidates(table, tree, filter, storage)) yield return generic;
        yield return new(AccessKind.Full, table.RowCount, tree.NodeCount, 0, ScanCost(table.RowCount, tree.NodeCount, filter), Scan: new());
    }
    private static int Height(GistNode node) => node.Leaf ? 1 : 1 + Height(node.Entries[0].Child!);
    private static long Descent(GistNode node, GistAccess access)
    {
        if (node.Leaf) return 0;
        long count = 1;
        foreach (var entry in node.Entries)
            if (entry.Bound.Matches([access.Query], [access.Strategy], false)) count = Estimate.Add(count, Descent(entry.Child!, access));
        return count;
    }
    internal async ValueTask Prime(CatalogTable table, StorageTree tree, QueryStorage storage, Meter meter, IReadOnlySet<int>? touched, CancellationToken token)
    {
        if (IndexSet is not null) return;
        if (Btree is not null)
        {
            if (!Btree.Scan.Empty) meter.Charge(Btree.Tree.CountRangeNodes(Btree.Scan.Lower, Btree.Scan.LowerInclusive, Btree.Scan.Upper, Btree.Scan.UpperInclusive));
        }
        else if (Gin is not null)
        {
            foreach (var arm in Gin.Arms) _ = await GinGather.Run(Gin.Tree, arm, meter, token).ConfigureAwait(false);
        }
        else if (Gist is not null)
        {
            if (!Gist.Query.IsNull)
            {
                var gist = await storage.Database.GistAsync(table, Gist.Index, storage.Pending, storage.Snapshot, token).ConfigureAwait(false);
                _ = gist.Probe([Gist.Query], [Gist.Strategy], meter);
            }
        }
        else
        {
            if (Streaming && Ranges is not null) return;
            var ranges = Ranges ?? [Scan ?? new()];
            foreach (var range in ranges)
                await ChargeRange(tree, range, meter, touched, token).ConfigureAwait(false);
        }
        meter.Checkpoint(token);
    }
    private static async ValueTask ChargeRange(StorageTree tree, TableScan range, Meter meter, IReadOnlySet<int>? touched, CancellationToken token)
    {
        if (range.Empty) return;
        meter.Charge(await tree.CountRangePagesAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, token).ConfigureAwait(false));
        await foreach (var record in tree.ScanRangeAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, cancellationToken: token).ConfigureAwait(false))
            meter.Charge(tree.ValueReadUnits(record, touched));
    }
    internal async IAsyncEnumerable<SqlValue[]> Rows(CatalogTable table, StorageTree tree, QueryStorage storage, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        await foreach (var record in Records(table, tree, storage, meter, touched, token, reverse).ConfigureAwait(false))
            yield return tree.DecodeDeferredRow(table, record);
    }
    internal async IAsyncEnumerable<StorageRecord> Records(CatalogTable table, StorageTree tree, QueryStorage storage, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        if (IndexSet is not null)
        {
            await foreach (var record in IndexSet.Records(table, tree, storage.Catalog, meter, touched, Streaming || Ordered, token).ConfigureAwait(false)) yield return record;
            yield break;
        }
        if (Btree is not null)
        {
            await foreach (var row in (Ordered ? Btree.OrderedRecords(table, tree, storage.Catalog, meter, touched, token) : Btree.Records(table, tree, storage.Catalog, meter, touched, token, reverse)).ConfigureAwait(false)) yield return row;
            yield break;
        }
        if (Gist is not null)
        {
            await foreach (var row in Gist.Records(table, tree, meter, touched, token, reverse).ConfigureAwait(false)) yield return row;
            yield break;
        }
        if (Gin is not null)
        {
            await foreach (var row in Gin.Records(tree, meter, touched, token, reverse).ConfigureAwait(false)) yield return row;
            yield break;
        }
        var ranges = Ranges ?? [Scan ?? new()];
        if (Streaming && Ranges is not null)
        {
            foreach (var range in reverse ? ranges.Reverse() : ranges)
            {
                if (range.Empty) continue;
                await ChargeRange(tree, range, meter, touched, token).ConfigureAwait(false);
                await foreach (var record in tree.ScanRangeAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, reverse, token).ConfigureAwait(false))
                { meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token); yield return record; }
            }
            yield break;
        }
        meter.Checkpoint(token);
        // All structural and touched-value units precede the first emitted row,
        // including for a disjoint interval set and a consumer that stops early.
        foreach (var range in ranges)
        {
            if (range.Empty) continue;
            meter.Charge(await tree.CountRangePagesAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, token).ConfigureAwait(false));
            await foreach (var record in tree.ScanRangeAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, cancellationToken: token).ConfigureAwait(false))
                meter.Charge(tree.ValueReadUnits(record, touched));
        }
        meter.Checkpoint(token);
        foreach (var range in reverse ? ranges.Reverse() : ranges)
        {
            if (range.Empty) continue;
            await foreach (var record in tree.ScanRangeAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, reverse, token).ConfigureAwait(false))
            { meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token); yield return record; }
        }
    }
}
