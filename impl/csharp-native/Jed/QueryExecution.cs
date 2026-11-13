using System.Runtime.CompilerServices;

namespace Jed;

internal static class QueryExecution
{
    internal static async IAsyncEnumerable<SqlValue[]> EmptySource()
    {
        await ValueTask.CompletedTask.ConfigureAwait(false); yield break;
    }
    internal static async IAsyncEnumerable<SqlValue[]> ScalarSource()
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        yield return [];
    }
    internal static async IAsyncEnumerable<SqlValue[]> TableSource(CatalogTable table, StorageTree tree, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken, TableScan? scan = null, IReadOnlySet<int>? touched = null)
    {
        scan ??= new();
        if (scan.Empty) yield break;
        meter.Checkpoint();
        meter.Charge(await tree.CountRangePagesAsync(scan.Lower, scan.LowerInclusive, scan.Upper, scan.UpperInclusive, cancellationToken).ConfigureAwait(false));
        await foreach (var record in tree.ScanRangeAsync(scan.Lower, scan.LowerInclusive, scan.Upper, scan.UpperInclusive, cancellationToken: cancellationToken).ConfigureAwait(false))
            meter.Charge(tree.ValueReadUnits(record, touched));
        meter.Checkpoint();
        await foreach (var record in tree.ScanRangeAsync(scan.Lower, scan.LowerInclusive, scan.Upper, scan.UpperInclusive, scan.Reverse, cancellationToken).ConfigureAwait(false))
        {
            meter.Charge(Costs.StorageRowRead); meter.Checkpoint();
            yield return tree.DecodeDeferredRow(table, record);
        }
    }
    internal static async IAsyncEnumerable<SqlValue[]> DeferredTableSource(CatalogTable table, StorageTree tree, SelectPlan plan, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var initialized = PlanTransform.InitialValues(plan);
        var access = TableAccess.Prepare(initialized, table);
        var touched = StorageTouchedColumns.Collect(initialized);
        var ranges = PointAccess.Ranges(initialized.Filter, table);
        if (ranges is null)
        {
            await foreach (var row in TableSource(table, tree, meter, cancellationToken, access.Scan, touched).ConfigureAwait(false)) yield return row;
            yield break;
        }
        foreach (var range in ranges)
        {
            meter.Charge(await tree.CountRangePagesAsync(range.Lower, true, range.Upper, true, cancellationToken).ConfigureAwait(false));
            await foreach (var record in tree.ScanRangeAsync(range.Lower, true, range.Upper, true, cancellationToken: cancellationToken).ConfigureAwait(false))
                meter.Charge(tree.ValueReadUnits(record, touched));
        }
        meter.Checkpoint();
        if (access.Scan.Reverse) Array.Reverse(ranges);
        foreach (var range in ranges)
            await foreach (var record in tree.ScanRangeAsync(range.Lower, true, range.Upper, true, cancellationToken: cancellationToken).ConfigureAwait(false))
            {
                meter.Charge(Costs.StorageRowRead); meter.Checkpoint(); yield return tree.DecodeDeferredRow(table, record);
            }

    }
    internal static async IAsyncEnumerable<Row> Select(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        plan = PlanTransform.InitialValues(plan);
        if (plan.Filter is not null && Resolve.EmptyFilter(plan.Filter)) plan = plan with { Empty = true };
        if (plan.Group is { } grouping)
        {
            source = GroupExecution.Run(grouping, plan.Empty ? EmptySource() : source, plan.Filter, meter, cancellationToken);
            plan = plan with { Filter = null, Empty = false, Group = null };
            // Aggregates finalize even when LIMIT excludes every output row.
            if (plan.Limit == 0 && !plan.Distinct)
            {
                await foreach (var unused in source.WithCancellation(cancellationToken).ConfigureAwait(false)) { }
                yield break;
            }
        }
        if (plan.Windows is { } windows)
        {
            if (plan.WindowPrefix)
            {
                var count = plan.Offset > long.MaxValue - plan.Limit!.Value ? long.MaxValue : plan.Offset + plan.Limit.Value;
                source = WindowPrefix.Take(plan.Empty ? EmptySource() : source, plan.Filter, count, meter, cancellationToken);
                plan = plan with { Filter = null, Empty = false };
            }
            source = WindowExecution.Run(windows, plan.Empty ? EmptySource() : source, plan.Filter, meter, cancellationToken);
            plan = plan with { Filter = null, Empty = false, Windows = null };
            if (plan.Limit == 0)
            {
                await foreach (var unused in source.WithCancellation(cancellationToken).ConfigureAwait(false)) { }
                yield break;
            }
        }
        if (plan.Empty) yield break;
        if (plan.Distinct)
        {
            await foreach (var row in DistinctExecution.Run(plan, source, meter, cancellationToken).ConfigureAwait(false)) yield return row;
            yield break;
        }
        var order = plan.Order ?? [];
        if (plan.Limit == 0 && order.Length == 0) yield break;
        long skipped = 0, produced = 0;
        if (order.Length != 0)
        {
            var rows = new List<(SqlValue[] Row, SqlValue[] Keys, int Ordinal)>();
            await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
            {
                if (!await KeepsAsync(plan.Filter, row, meter).ConfigureAwait(false)) continue;
                var keys = new SqlValue[order.Length];
                for (var i = 0; i < keys.Length; i++)
                {
                    keys[i] = await Evaluate.ExprAsync(order[i].Expression, meter, row: row).ConfigureAwait(false);
                    var work = KeyBytes(keys[i]);
                    if (work != 0) { meter.Charge(work * Costs.SortKey); meter.Guard(); }
                }
                rows.Add((row, keys, rows.Count));
            }
            rows.Sort((left, right) =>
            {
                for (var i = 0; i < order.Length; i++)
                {
                    var a = left.Keys[i]; var b = right.Keys[i];
                    var compare = a.IsNull || b.IsNull ? a.IsNull == b.IsNull ? 0 : (a.IsNull == order[i].NullsFirst ? -1 : 1) :
                        order[i].Descending ? Evaluate.Compare(b, a) : Evaluate.Compare(a, b);
                    if (compare != 0) return compare;
                }
                return left.Ordinal.CompareTo(right.Ordinal);
            });
            foreach (var row in rows)
            {
                if (skipped++ < plan.Offset) continue;
                if (plan.Limit is { } limit && produced >= limit) break;
                produced++; yield return await ProjectAsync(plan.Projections, row.Row, meter, plan.ChargeRows).ConfigureAwait(false);
            }
            yield break;
        }
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await KeepsAsync(plan.Filter, row, meter).ConfigureAwait(false)) continue;
            if (skipped++ < plan.Offset) continue;
            produced++; yield return await ProjectAsync(plan.Projections, row, meter, plan.ChargeRows).ConfigureAwait(false);
            if (plan.Limit is { } limit && produced >= limit) yield break;
        }
    }
    internal static async ValueTask<bool> KeepsAsync(Resolved? filter, SqlValue[] row, Meter meter)
    {
        if (filter is null) return true;
        var result = await Evaluate.ExprAsync(filter, meter, true, row).ConfigureAwait(false);
        return !result.IsNull && result.AsBoolean();
    }
    internal static async ValueTask<Row> ProjectAsync(Projection[] projections, SqlValue[] row, Meter meter, bool chargeRow = true)
    {
        meter.Checkpoint(); if (chargeRow) meter.Charge(Costs.RowProduced);
        var values = new SqlValue[projections.Length];
        for (var i = 0; i < values.Length; i++) values[i] = await Evaluate.ExprAsync(projections[i].Expression, meter, row: row).ConfigureAwait(false);
        meter.Checkpoint(); return new(values);
    }
    internal static bool Keeps(Resolved? filter, SqlValue[] row, Meter meter)
    {
        if (filter is null) return true;
        var result = Evaluate.Expr(filter, meter, true, row);
        return !result.IsNull && result.AsBoolean();
    }
    internal static Row Project(Projection[] projections, SqlValue[] row, Meter meter, bool chargeRow = true)
    {
        meter.Checkpoint(); if (chargeRow) meter.Charge(Costs.RowProduced);
        var values = new SqlValue[projections.Length];
        for (var i = 0; i < values.Length; i++) values[i] = Evaluate.Expr(projections[i].Expression, meter, row: row);
        meter.Checkpoint(); return new(values);
    }
    internal static long KeyBytes(SqlValue value) => OpenValue.PayloadBytes(value);
    internal static async IAsyncEnumerable<Row> Materialized(IEnumerable<Row> rows)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        foreach (var row in rows) yield return row;
    }
}
