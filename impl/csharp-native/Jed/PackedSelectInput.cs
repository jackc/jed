using System.Runtime.CompilerServices;

namespace Jed;

// Simple fixed-table queries share the ordinary expression kernels but consume
// page chunks directly, without an async iterator handoff for every input row.
internal sealed record PackedSelectInput(CatalogTable Table, StorageTree Tree, TableScan Scan, bool Reverse, int[] ProjectedSlots, bool Identity)
{
    internal static PackedSelectInput? Create(SelectPlan plan, CatalogTable table, StorageTree tree)
    {
        if (plan.Group is not null || plan.Windows is not null || plan.Distinct || plan.DistinctOnCount != 0 ||
            plan.Order is { Length: > 0 } || plan.BeforeZeroLimit is not null || plan.Observation is not null ||
            plan.Access is not { Kind: AccessKind.Primary or AccessKind.Full, Ranges: null, IndexSet: null, Btree: null, Gist: null, Gin: null } access ||
            tree.HasValueReadUnits() || PlanTransform.DynamicAccess(plan.Filter) ||
            !Resident(plan.Filter) || plan.Projections.Any(projection => !Resident(projection.Expression)) ||
            plan.ColumnarProjection && (plan.Limit is not null || plan.Offset != 0 || plan.Projections.Any(projection => projection.Expression.Op != "column"))) return null;
        var scan = access.Scan ?? new();
        var slots = plan.ColumnarProjection ? plan.Projections.Select(projection => projection.Expression.Slot).Distinct().ToArray() : [];
        var identity = plan.Projections.Length == table.Columns.Length;
        for (var i = 0; identity && i < plan.Projections.Length; i++)
            identity = plan.Projections[i].Expression is { Op: "column" } column && column.Slot == i;
        return new(table, tree, scan, plan.ScanReverse, slots, identity);
    }

    private static bool Resident(Resolved? expression) => expression is null ||
        expression.Subquery is null && expression.Outer is null && expression.Json is null &&
        expression.ParameterIndex == 0 && !expression.ParameterUnbound && !expression.BoundSource &&
        !(expression.Op == "constant" && expression.Constant.IsDeferred) && AggregateLanePlan.DiscardSafe(expression) &&
        expression.Args.All(Resident) && (expression.GrowthOperands?.All(Resident) ?? true);

    internal IAsyncEnumerable<Row> Run(SelectPlan plan, Meter meter, CancellationToken token) =>
        KeyIntervals.Point(Scan) ? Point(plan, meter, token) : Range(plan, meter, token);

    private async IAsyncEnumerable<Row> Point(SelectPlan plan, Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        if (plan.Empty || plan.Filter is not null && Resolve.EmptyFilter(plan.Filter) || plan.Limit == 0) yield break;
        using var memory = new QueryInputMemory(meter);
        meter.Checkpoint(token);
        if (!Scan.Empty)
            meter.Charge(await Tree.CountRangePagesAsync(Scan.Lower, Scan.LowerInclusive, Scan.Upper, Scan.UpperInclusive, token).ConfigureAwait(false));
        meter.Checkpoint(token);
        SqlValue[]? row = null;
        if (!Scan.Empty && await Tree.FindRowAsync(Scan.Lower!, token).ConfigureAwait(false) is { } found)
        {
            token.ThrowIfCancellationRequested();
            meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
            var decoded = found.DecodeFixed(Table, Tree);
            if (QueryExecution.Keeps(plan.Filter, decoded, meter)) row = decoded;
        }
        if (plan.ColumnarProjection)
        {
            long bytes = 0;
            if (row is not null && meter.Memory.Enabled)
                foreach (var slot in ProjectedSlots) bytes = Meter.Add(bytes, MemorySize.ValueBytes(row[slot]));
            memory.ReserveBytes(bytes);
        }
        if (row is not null && plan.Offset == 0)
            yield return QueryExecution.Project(plan.Projections, row, meter, plan.ChargeRows, ownedIdentity: Identity);
    }

    private async IAsyncEnumerable<Row> Range(SelectPlan plan, Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        if (plan.Empty || plan.Filter is not null && Resolve.EmptyFilter(plan.Filter) || plan.Limit == 0) yield break;
        using var memory = new QueryInputMemory(meter);
        var buffered = plan.ColumnarProjection ? new List<SqlValue[]>() : null;
        long bytes = 0, skipped = 0, produced = 0;
        meter.Checkpoint(token);
        if (!Scan.Empty)
            meter.Charge(await Tree.CountRangePagesAsync(Scan.Lower, Scan.LowerInclusive, Scan.Upper, Scan.UpperInclusive, token).ConfigureAwait(false));
        meter.Checkpoint(token);
        if (!Scan.Empty)
            await foreach (var chunk in Tree.ScanRangeChunksAsync(Scan.Lower, Scan.LowerInclusive, Scan.Upper, Scan.UpperInclusive, Reverse, token).ConfigureAwait(false))
                for (var index = Reverse ? chunk.Until - 1 : chunk.From; Reverse ? index >= chunk.From : index < chunk.Until; index += Reverse ? -1 : 1)
                {
                    token.ThrowIfCancellationRequested();
                    meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
                    var row = chunk.Row(index).DecodeFixed(Table, Tree);
                    if (!QueryExecution.Keeps(plan.Filter, row, meter)) continue;
                    if (buffered is not null)
                    {
                        if (meter.Memory.Enabled)
                            foreach (var slot in ProjectedSlots) bytes = Meter.Add(bytes, MemorySize.ValueBytes(row[slot]));
                        buffered.Add(row); continue;
                    }
                    if (skipped++ < plan.Offset) continue;
                    produced++;
                    yield return QueryExecution.Project(plan.Projections, row, meter, plan.ChargeRows, ownedIdentity: Identity);
                    if (plan.Limit is { } limit && produced >= limit) yield break;
                }
        if (buffered is null) yield break;
        // The columnar projection contract is blocking: admission occurs once,
        // after scan/filter and before any projection or public output.
        memory.ReserveBytes(bytes);
        foreach (var row in buffered)
            yield return QueryExecution.Project(plan.Projections, row, meter, plan.ChargeRows, ownedIdentity: Identity);
    }
}
