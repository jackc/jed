using System.Runtime.CompilerServices;

namespace Jed;

// One fixed-width page feeds a reusable row and the ordinary finite folds.
// Only the storage traversal suspends; SQL evaluation and metering stay shared.
internal sealed record PackedAggregateInput(CatalogTable Table, StorageTree Tree, TableScan Scan, bool Reverse, long MaximumStateBytes)
{
    internal PackedNumericAggregate? Numeric { get; init; }
    internal static PackedAggregateInput? Create(SelectPlan plan, CatalogTable table, StorageTree tree)
    {
        if (!plan.AggregateLane || plan.Group is not { Keys.Length: 0 } group || tree.HasValueReadUnits() ||
            PlanTransform.DynamicAccess(plan.Filter) || !AggregateLanePlan.DiscardSafe(plan.Filter) ||
            group.Aggregates.Any(aggregate => aggregate.Argument is { } argument && !argument.Type.IsInteger() && !argument.Type.IsFloat())) return null;
        // Empty key entry + ordinal + each packed accumulator. Integer decimal
        // sums never exceed 39 digits (Int64 inputs and an Int64 count).
        long maximum = MemorySchedule.Entry + MemorySchedule.Value;
        foreach (var aggregate in group.Aggregates)
        {
            var size = new SpillFold(aggregate).LogicalBytes();
            if (aggregate.Name is "sum" or "avg" && aggregate.Type == SqlType.Decimal)
                size += 10 * MemorySchedule.DecimalGroup;
            maximum = Meter.Add(maximum, size);
        }
        return new(table, tree, plan.Access?.Scan ?? new(), plan.ScanReverse, maximum) { Numeric = PackedNumericAggregate.Create(plan, table) };
    }

    internal bool Eligible(SelectPlan plan, Meter meter) => !meter.Memory.Enabled &&
        (plan.Spill is not { WorkMem: > 0 } spill || MaximumStateBytes <= spill.WorkMem);

    internal async IAsyncEnumerable<SqlValue[]> Run(SelectPlan plan, Meter meter,
        [EnumeratorCancellation] CancellationToken token)
    {
        var aggregates = plan.Group!.Aggregates;
        var folds = aggregates.Select(aggregate => new GroupExecution.Fold(aggregate)).ToArray();
        var values = new SqlValue[Table.Columns.Length];
        var numericColumns = Numeric?.Scratch();
        long folded = 0;
        SqlException? filterError = null;
        var settleErrors = !AggregateLanePlan.RequiresAttempt(plan, meter);
        if (!Scan.Empty && !plan.Empty)
        {
            meter.Checkpoint(token);
            meter.Charge(await Tree.CountRangePagesAsync(Scan.Lower, Scan.LowerInclusive, Scan.Upper, Scan.UpperInclusive, token).ConfigureAwait(false));
            meter.Checkpoint(token);
            await foreach (var chunk in Tree.ScanRangeChunksAsync(Scan.Lower, Scan.LowerInclusive, Scan.Upper, Scan.UpperInclusive, Reverse, token).ConfigureAwait(false))
            {
                if (Numeric is not null && chunk.Leaf is not null)
                { Numeric.Run(chunk, numericColumns!, folds, meter, token); continue; }
                for (var index = Reverse ? chunk.Until - 1 : chunk.From; Reverse ? index >= chunk.From : index < chunk.Until; index += Reverse ? -1 : 1)
                {
                    meter.Charge(Costs.StorageRowRead); meter.Checkpoint(token);
                    chunk.DecodeFixedInto(Table, Tree, index, values);
                    if (filterError is not null) continue;
                    bool keep;
                    try { keep = QueryExecution.Keeps(plan.Filter, values, meter); }
                    catch (SqlException error) when (settleErrors) { filterError = error; continue; }
                    if (!keep) continue;
                    var before = meter.Accrued;
                    for (var aggregate = 0; aggregate < aggregates.Length; aggregate++)
                    {
                        var operation = aggregates[aggregate];
                        meter.Charge(Costs.AggregateAccumulate); meter.Guard();
                        var value = operation.Argument is null ? SqlValue.Integer(1) : Evaluate.Expr(operation.Argument, meter, row: values);
                        GroupExecution.Accumulate(operation, folds[aggregate], value, meter);
                    }
                    folded = Meter.Add(folded, meter.Accrued - before);
                }
            }
        }
        if (filterError is not null) { meter.Refund(folded); throw filterError; }
        var result = new SqlValue[aggregates.Length];
        for (var index = 0; index < aggregates.Length; index++) result[index] = GroupExecution.Finalize(aggregates[index], folds[index]);
        if (plan.Spill is { } spill)
        {
            await using var output = new SpillRowSpool(spill.Host, spill.WorkMem, meter);
            await output.AppendOwned(result, token).ConfigureAwait(false);
            await foreach (var row in output.Read(token).ConfigureAwait(false)) yield return row;
        }
        else yield return result;
    }
}
