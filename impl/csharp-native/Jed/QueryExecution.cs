using System.Runtime.CompilerServices;

namespace Jed;

internal static class QueryExecution
{
    internal static async IAsyncEnumerable<SqlValue[]> EmptySource()
    {
        await ValueTask.CompletedTask.ConfigureAwait(false); yield break;
    }
    internal static async IAsyncEnumerable<SqlValue[]> ScalarSource(Meter meter)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        var bytes = meter.Memory.ReserveRow(Array.Empty<SqlValue>());
        try { yield return []; }
        finally { meter.Memory.Release(bytes); }
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
        [EnumeratorCancellation] CancellationToken cancellationToken, QueryStorage? storage = null)
    {
        var dynamic = PlanTransform.DynamicAccess(plan.Filter);
        var initialized = PlanTransform.InitialValues(plan);
        if (dynamic) initialized = initialized with { Filter = await PlanTransform.AccessValues(plan.Filter!, cancellationToken).ConfigureAwait(false) };
        if (storage is { MembershipBounds: false } && initialized.Filter is not null) initialized = initialized with { Filter = PlanTransform.WithoutMembershipBounds(initialized.Filter) };
        var access = TableAccess.Prepare(initialized, table);
        var touched = StorageTouchedColumns.Collect(initialized);
        if (storage is not null)
        {
            var selected = !dynamic && initialized.Access is { } retained ? retained : SinglePipeline.Select(table, tree, initialized, storage, meter).Access;
            if (dynamic && PlanTransform.CorrelatedAccess(plan.Filter) && initialized.Access is { } pinned)
                selected = AccessPlan.Rebind(pinned, table, tree, initialized.Filter, storage, meter);
            if (dynamic && !PlanTransform.CorrelatedAccess(plan.Filter) && initialized.AccessOrdering && initialized.Access is { } ordered)
            {
                // Rebinding may narrow the walk but must preserve an order whose
                // explicit Sort was removed when this query frame was prepared.
                var compatible = AccessPlan.Inventory(table, tree, initialized.Filter, storage, meter).Where(candidate =>
                    ordered.Ordered ? candidate.Index == ordered.Index && candidate.Kind is AccessKind.Btree or AccessKind.IndexIntervals :
                    candidate.Kind is AccessKind.Primary or AccessKind.PrimaryIntervals or AccessKind.Full);
                selected = compatible.OrderBy(candidate => candidate.EstimatedCost).First() with { Ordered = ordered.Ordered, Streaming = ordered.Streaming };
            }
            await foreach (var row in selected.Rows(table, tree, storage, meter, touched, cancellationToken, initialized.Access is null ? access.Scan.Reverse : initialized.ScanReverse,
                meter.HostBatches.ScanFor(initialized)).ConfigureAwait(false)) yield return row;
        }
        else
        {
            await foreach (var row in TableSource(table, tree, meter, cancellationToken, access.Scan, touched).ConfigureAwait(false)) yield return row;
        }
    }

    internal static async IAsyncEnumerable<Row> Select(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken = default, bool sourceMemory = false, bool eagerInput = false,
        bool transferIdentity = false, bool bufferOutput = false)
    {
        var execution = SelectCore(plan, source, meter, cancellationToken, sourceMemory, eagerInput, transferIdentity);
        if (!bufferOutput)
        {
            await foreach (var row in execution.ConfigureAwait(false)) yield return row;
            yield break;
        }
        using var memory = new QueryInputMemory(meter); var rows = new List<SqlValue[]>();
        await foreach (var row in execution.ConfigureAwait(false))
        {
            var values = Enumerable.Range(0, row.Count).Select(i => row[i]).ToArray();
            memory.Take(values, transferIdentity); rows.Add(values);
        }
        foreach (var row in rows) { memory.Transfer(row); yield return new Row(row); }
    }

    private static async IAsyncEnumerable<Row> SelectCore(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source, Meter meter,
        [EnumeratorCancellation] CancellationToken cancellationToken = default, bool sourceMemory = false, bool eagerInput = false, bool transferIdentity = false)
    {
        using var inputMemory = new QueryInputMemory(meter);
        var retainProjectedSource = plan.Group is not null || plan.Windows is not null;
        var groupSpool = plan.Group is not null && plan.Spill is not null && plan.Windows is null;
        var retainInput = eagerInput && plan.Group is null && plan.Windows is null && !plan.Distinct;
        if (retainInput)
        {
            source = EagerSource(source, inputMemory, sourceMemory, StorageTouchedColumns.Collect(plan), cancellationToken);
            sourceMemory = true; retainProjectedSource = true;
        }
        else if (sourceMemory && plan.Group is null && plan.Windows is null) source = inputMemory.Adopt(source, cancellationToken);
        // Correlated values bind storage bounds per outer row, but do not
        // turn a non-key residual into a new statement-wide contradiction.
        var correlatedFilter = plan.Filter is not null && ContainsOuter(plan.Filter);
        plan = PlanTransform.InitialValues(plan);
        using var hostScan = meter.HostBatches.BeginScan(plan, !sourceMemory && !eagerInput);
        if (!correlatedFilter && plan.Filter is not null && Resolve.EmptyFilter(plan.Filter)) plan = plan with { Empty = true };
        if (plan.ColumnarProjection)
        {
            source = ColumnarSource(plan, source, meter, cancellationToken);
            plan = plan with { Filter = null };
        }
        if (plan.Group is { } grouping)
        {
            var grouped = GroupExecution.Run(grouping, plan.Empty ? EmptySource() : source, plan.Filter, meter, cancellationToken, sourceMemory, StorageTouchedColumns.Collect(plan), plan.AggregateLane, plan.Observation?.Filter, plan.Spill);
            source = plan.Spill is null ? inputMemory.Adopt(grouped, cancellationToken) : grouped;
            if (plan.Observation?.Aggregate is { } groupObservation) source = ExplainObservation.Rows(source, groupObservation, meter, cancellationToken, inclusive: true);
            sourceMemory = plan.Spill is null;
            retainProjectedSource = sourceMemory;
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
            source = inputMemory.Adopt(WindowExecution.Run(windows, plan.Empty ? EmptySource() : source, plan.Filter, meter, cancellationToken, sourceMemory, StorageTouchedColumns.Collect(plan), plan.Observation?.Filter), cancellationToken);
            if (plan.Observation?.Window is { } windowObservation) source = ExplainObservation.Rows(source, windowObservation, meter, cancellationToken, inclusive: true);
            sourceMemory = true;
            plan = plan with { Filter = null, Empty = false, Windows = null };
            if (plan.Limit == 0)
            {
                await foreach (var unused in source.WithCancellation(cancellationToken).ConfigureAwait(false)) { }
                yield break;
            }
        }
        if (plan.Empty) yield break;
        if (plan.Distinct && plan.DistinctOnCount == 0)
        {
            await foreach (var row in DistinctExecution.Run(plan, source, meter, cancellationToken, sourceMemory, inputMemory).ConfigureAwait(false)) yield return row;
            yield break;
        }
        var order = plan.Order ?? [];
        if (plan.Limit == 0 && order.Length == 0)
        {
            if (plan.BeforeZeroLimit is { } prime) await prime().ConfigureAwait(false);
            yield break;
        }
        long skipped = 0, produced = 0;
        if (order.Length != 0)
        {
            using var sortMemory = new QueryInputMemory(meter);
            using var decorations = new QueryInputMemory(meter);
            var rows = new List<(SqlValue[] Row, SortValue[] Keys, int Ordinal)>();
            var heap = !sourceMemory && !plan.Distinct && plan.Group is null && plan.Windows is null && plan.Limit is { } heapLimit && (heapLimit == 0 || heapLimit <= long.MaxValue - plan.Offset);
            if (heap && plan.Spill is { } budget) heap = budget.TopK(plan);
            if (!heap && plan.Spill is { } spill)
            {
                await foreach (var row in SpillSort.Run(plan, source, spill, inputMemory, meter, cancellationToken).ConfigureAwait(false)) yield return row;
                yield break;
            }
            var collatedHeap = heap && order.Any(key => key.Expression.Collation?.Table is not null);
            if (!sourceMemory && order.Any(key => key.Expression.Collation?.Table is not null))
            {
                var survivors = new List<SqlValue[]>();
                await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
                    if (await KeepsAsync(plan.Filter, row, meter, plan.Observation?.Filter).ConfigureAwait(false))
                        survivors.Add(await sortMemory.TakeAsync(row, false, StorageTouchedColumns.Collect(plan)).ConfigureAwait(false));
                source = BufferedSource(survivors); plan = plan with { Filter = null };
                if (heap) sortMemory.Dispose(); else sourceMemory = true;
            }
            var capacity = plan.Limit == 0 ? 0 : plan.Limit is { } take ? Estimate.Add(plan.Offset, take) : long.MaxValue;
            var ordinal = 0;
            int Compare((SqlValue[] Row, SortValue[] Keys, int Ordinal) left, (SqlValue[] Row, SortValue[] Keys, int Ordinal) right)
            {
                for (var i = 0; i < order.Length; i++)
                {
                    var compared = SortValue.Compare(left.Keys[i], right.Keys[i], order[i]);
                    if (compared != 0) return compared;
                }
                return left.Ordinal.CompareTo(right.Ordinal);
            }
            void ReleaseKeys(SortValue[] keys)
            {
                if (!meter.Memory.Enabled || heap && capacity == 0) return;
                for (var i = 0; i < keys.Length; i++)
                    if (SortPlan.Column(order[i].Expression) is null) sortMemory.ReleaseBytes(MemorySize.ValueBytes(keys[i].Value));
            }
            await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
            {
                if (!await KeepsAsync(plan.Filter, row, meter, plan.Observation?.Filter).ConfigureAwait(false)) { inputMemory.Release(row); continue; }
                if (heap && capacity == 0 && order.All(key => SortPlan.Column(key.Expression) is not null && key.Expression.Collation?.Table is null)) continue;
                var keyCost = meter.Accrued;
                var keys = new SortValue[order.Length];
                for (var i = 0; i < keys.Length; i++)
                {
                    var value = await Evaluate.ExprAsync(order[i].Expression, meter, row: row).ConfigureAwait(false);
                    if (SortPlan.Column(order[i].Expression) is null && !(heap && capacity == 0)) sortMemory.ReserveValue(value);
                    keys[i] = CollationRules.Decorate(value, order[i].Expression, meter);
                }
                plan.Observation?.Sort?.AttributeLocal(meter.Accrued - keyCost);
                if (heap && capacity == 0) { ReleaseKeys(keys); continue; }
                var candidate = (Row: row, Keys: keys, Ordinal: ordinal++);
                if (heap && rows.Count >= capacity && Compare(candidate, rows[^1]) >= 0) { ReleaseKeys(keys); continue; }
                var buffered = sourceMemory || collatedHeap ? row : await sortMemory.TakeAsync(row, false, StorageTouchedColumns.Collect(plan)).ConfigureAwait(false);
                if (heap && rows.Count >= capacity) sortMemory.Release(rows[^1].Row);
                decorations.ReserveBytes(MemorySize.SortKeyBytes(keys));
                if (heap && rows.Count >= capacity)
                {
                    ReleaseKeys(rows[^1].Keys);
                    decorations.ReleaseBytes(MemorySize.SortKeyBytes(rows[^1].Keys));
                    rows.RemoveAt(rows.Count - 1);
                }
                rows.Add((buffered, keys, candidate.Ordinal));
                if (heap)
                {
                    rows.Sort(Compare);
                }
            }
            if (!heap) rows.Sort(Compare);
            decorations.Dispose();
            if (collatedHeap) foreach (var row in rows) sortMemory.Reserve(row.Row);
            using var hostBatch = HostBatchReplay.HasEligible(plan.Projections.Select(projection => projection.Expression))
                ? meter.HostBatches.Begin(plan.Projections.Select(projection => projection.Expression),
                    rows.Where((_, index) => index >= plan.Offset && (plan.Limit is null || index - plan.Offset < plan.Limit.Value)).Select(row => row.Row).ToArray()) : null;
            SortValue[]? previous = null;
            foreach (var row in rows)
            {
                if (plan.DistinctOnCount != 0)
                {
                    var same = previous is not null && Enumerable.Range(0, plan.DistinctOnCount).All(i => SortValue.Compare(previous[i], row.Keys[i], order[i]) == 0);
                    if (same) { sortMemory.Release(row.Row); inputMemory.Release(row.Row); continue; }
                    previous = row.Keys;
                }
                if (skipped++ < plan.Offset) continue;
                if (plan.Limit is { } limit && produced >= limit) break;
                produced++; yield return await ProjectAsync(plan.Projections, row.Row, meter, plan.ChargeRows).ConfigureAwait(false);
            }
            yield break;
        }
        IDisposable? bufferedHostBatch = null;
        var bufferedRoots = (plan.Filter is { } bufferedFilter ? new[] { bufferedFilter } : [])
            .Concat(plan.Projections.Select(projection => projection.Expression)).ToArray();
        if (retainInput && HostBatchReplay.HasEligible(bufferedRoots))
        {
            var buffered = new List<SqlValue[]>();
            await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false)) buffered.Add(row);
            source = BufferedSource(buffered);
            bufferedHostBatch = meter.HostBatches.Begin(bufferedRoots, buffered);
        }
        using var hostBatchScope = bufferedHostBatch;
        if (groupSpool)
            source = HostPullWindow.Read(source, bufferedRoots, meter, Costs.RowProduced,
                () => plan.Limit is { } count ? Math.Max(0, Meter.Add(plan.Offset, count) - skipped) : null, cancellationToken);
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await KeepsAsync(plan.Filter, row, meter, plan.Observation?.Filter).ConfigureAwait(false)) { inputMemory.Release(row); continue; }
            hostScan?.Passed();
            if (skipped++ < plan.Offset) { inputMemory.Release(row); continue; }
            if (transferIdentity && sourceMemory)
            {
                if (plan.ChargeRows) meter.Charge(Costs.RowProduced);
                inputMemory.Transfer(row); produced++; yield return new Row(row);
                if (plan.Limit is { } identityLimit && produced >= identityLimit) yield break;
                continue;
            }
            var projected = await ProjectAsync(plan.Projections, row, meter, plan.ChargeRows).ConfigureAwait(false);
            if (!retainProjectedSource) inputMemory.Release(row);
            produced++; yield return projected;
            if (plan.Limit is { } limit && produced >= limit) yield break;
        }
    }
    private static async IAsyncEnumerable<SqlValue[]> BufferedSource(IEnumerable<SqlValue[]> rows)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        foreach (var row in rows) yield return row;
    }

    private static async IAsyncEnumerable<SqlValue[]> ColumnarSource(SelectPlan plan, IAsyncEnumerable<SqlValue[]> source, Meter meter,
        [EnumeratorCancellation] CancellationToken token)
    {
        using var memory = new QueryInputMemory(meter);
        var rows = new List<SqlValue[]>(); var slots = plan.Projections.Select(projection => projection.Expression.Slot).Distinct().ToArray();
        long bytes = 0;
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        {
            if (!await KeepsAsync(plan.Filter, row, meter, plan.Observation?.Filter).ConfigureAwait(false)) continue;
            foreach (var slot in slots)
            {
                row[slot] = await row[slot].ResolveAsync(token).ConfigureAwait(false);
                if (meter.Memory.Enabled) bytes = Meter.Add(bytes, MemorySize.ValueBytes(row[slot]));
            }
            rows.Add(row);
        }
        memory.ReserveBytes(bytes);
        foreach (var row in rows) yield return row;
    }

    private static async IAsyncEnumerable<SqlValue[]> EagerSource(IAsyncEnumerable<SqlValue[]> source, QueryInputMemory memory,
        bool transferred, HashSet<int> touched, [EnumeratorCancellation] CancellationToken token)
    {
        var rows = new List<SqlValue[]>();
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false)) rows.Add(await memory.TakeAsync(row, transferred, touched).ConfigureAwait(false));
        foreach (var row in rows) yield return row;
    }
internal static async ValueTask<bool> KeepsValueAsync(Resolved? expression, SqlValue[] row, Meter meter)
{
    if (expression is null) return true;
    var result = await Evaluate.ExprAsync(expression, meter, row: row).ConfigureAwait(false);
    return !result.IsNull && result.AsBoolean();
}
    internal static async ValueTask<bool> KeepsAsync(Resolved? filter, SqlValue[] row, Meter meter, ExplainNode? observation = null)
    {
        if (filter is null) return true;
        var before = meter.Accrued;
        try
        {
            var result = await Evaluate.ExprAsync(filter, meter, true, row).ConfigureAwait(false);
            return !result.IsNull && result.AsBoolean();
        }
        finally { observation?.AttributeLocal(meter.Accrued - before); }
    }
    internal static async ValueTask<Row> ProjectAsync(Projection[] projections, SqlValue[] row, Meter meter, bool chargeRow = true)
    {
        meter.Checkpoint(); if (chargeRow) meter.Charge(Costs.RowProduced);
        var values = new SqlValue[projections.Length];
        for (var i = 0; i < values.Length; i++) values[i] = await Evaluate.ExprAsync(projections[i].Expression, meter, row: row).ConfigureAwait(false);
        meter.CheckCancellation(); return new(values);
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
        meter.CheckCancellation(); return new(values);
    }
    internal static long KeyBytes(SqlValue value) => OpenValue.PayloadBytes(value);
    private static bool ContainsOuter(Resolved expression) => expression.Op == "outer" || expression.Args.Any(ContainsOuter);
    internal static async IAsyncEnumerable<Row> Materialized(IEnumerable<Row> rows)
    {
        await ValueTask.CompletedTask.ConfigureAwait(false);
        foreach (var row in rows) yield return row;
    }
}
