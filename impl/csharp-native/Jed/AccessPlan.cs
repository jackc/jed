using System.Runtime.CompilerServices;

namespace Jed;

internal enum AccessKind { Primary, Btree, Gist, Gin, PrimaryIntervals, IndexIntervals, Full }

// Retained physical facts shared by execution, join selection and EXPLAIN.
// Candidate estimates never inspect table values or charge the runtime meter.
internal sealed record AccessPlan(AccessKind Kind, long EstimatedRows, long EstimatedPages, long EstimatedEntries,
    long EstimatedCost, Resolved? Predicate = null, CatalogIndex? Index = null, TableScan? Scan = null,
    TableScan[]? Ranges = null, SecondaryAccess? Btree = null, GistAccess? Gist = null, GinAccess? Gin = null)
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

    internal static AccessPlan Select(CatalogTable table, StorageTree tree, Resolved? filter, QueryStorage storage, Meter meter) =>
        Inventory(table, tree, filter, storage, meter).OrderBy(candidate => candidate.EstimatedCost).First();

    internal static IEnumerable<AccessPlan> Inventory(CatalogTable table, StorageTree tree, Resolved? filter, QueryStorage storage, Meter meter)
    {
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
            yield return new(AccessKind.PrimaryIntervals, rows, pages, 0, ScanCost(rows, pages, filter), filter, Ranges: ranges);
        }
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
    internal async IAsyncEnumerable<SqlValue[]> Rows(CatalogTable table, StorageTree tree, QueryStorage storage, Meter meter,
        IReadOnlySet<int>? touched, [EnumeratorCancellation] CancellationToken token, bool reverse = false)
    {
        if (Btree is not null)
        {
            await foreach (var row in Btree.Rows(table, tree, storage.Catalog, meter, touched, token, reverse).ConfigureAwait(false)) yield return row;
        }
        else if (Gist is not null)
        {
            await foreach (var row in Gist.Rows(table, tree, meter, touched, token, reverse).ConfigureAwait(false)) yield return row;
        }
        else if (Gin is not null)
        {
            await foreach (var row in Gin.Rows(table, tree, storage.Catalog, meter, touched, token, reverse).ConfigureAwait(false)) yield return row;
        }
        else if (Ranges is not null)
        {
            // Page/decode work for the whole disjoint set precedes row delivery.
            foreach (var range in Ranges)
            {
                meter.Charge(await tree.CountRangePagesAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, token).ConfigureAwait(false));
                await foreach (var record in tree.ScanRangeAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, cancellationToken: token).ConfigureAwait(false))
                    meter.Charge(tree.ValueReadUnits(record, touched));
            }
            foreach (var range in reverse ? Ranges.Reverse() : Ranges)
                await foreach (var record in tree.ScanRangeAsync(range.Lower, range.LowerInclusive, range.Upper, range.UpperInclusive, reverse, token).ConfigureAwait(false))
                { meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token); yield return tree.DecodeDeferredRow(table, record); }
        }
        else
        {
            await foreach (var row in QueryExecution.TableSource(table, tree, meter, token, (Scan ?? new()) with { Reverse = reverse }, touched).ConfigureAwait(false)) yield return row;
        }
    }
}
