using System.Runtime.CompilerServices;

namespace Jed;

internal static class WindowExecution
{
    private sealed record PartitionSet(List<int[]> Partitions, SqlValue[][] OrderKeys, SortValue[][] Decorations);
    internal static async IAsyncEnumerable<SqlValue[]> Run(WindowStage stage, IAsyncEnumerable<SqlValue[]> source,
        Resolved? filter, Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken, bool sourceMemory = false, IReadOnlySet<int>? touched = null, ExplainNode? filterObservation = null)
    {
        using var memory = new QueryInputMemory(meter);
        using var partitions = new QueryInputMemory(meter);
        var rows = new List<SqlValue[]>();
        await foreach (var input in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(filter, input, meter, filterObservation).ConfigureAwait(false))
            { if (sourceMemory && meter.Memory.Enabled) meter.Memory.Release(MemorySize.RowBytes(input)); continue; }
            var row = new SqlValue[stage.InputWidth + stage.Plans.Length]; input.CopyTo(row, 0);
            if (meter.Memory.Enabled)
            {
                var before = sourceMemory ? MemorySize.RowBytes(input) : 0;
                for (var i = 0; i < input.Length; i++) row[i] = touched is not null && !touched.Contains(i) ? SqlValue.Null(input[i].Type) : await input[i].ResolveAsync(cancellationToken).ConfigureAwait(false);
                var bytes = MemorySize.RowBytes(row[..stage.InputWidth]);
                if (bytes > before) memory.ReserveBytes(bytes - before); else meter.Memory.Release(before - bytes);
                memory.AdoptBytes(Math.Min(bytes, before));
            }
            rows.Add(row);
        }
        var shared = new List<(WindowPlan Plan, PartitionSet Set)>();
        for (var spec = 0; spec < stage.Plans.Length; spec++)
        {
            var plan = stage.Plans[spec]; PartitionSet? set = null;
            foreach (var item in shared) if (SameKeys(plan, item.Plan)) { set = item.Set; break; }
            if (set is null) { set = await Partition(plan, rows, meter, partitions).ConfigureAwait(false); shared.Add((plan, set)); }
            foreach (var indices in set.Partitions)
            {
                var n = indices.Length; var peerStart = new int[n]; var peerEnd = new int[n]; var groupNumber = new int[n]; var starts = new List<int>();
                for (var i = 0; i < n;)
                {
                    var end = i + 1;
                    while (end < n && CompareKeys(plan.Order, set.Decorations[indices[i]], set.Decorations[indices[end]]) == 0) end++;
                    var group = starts.Count; starts.Add(i);
                    for (var at = i; at < end; at++) { peerStart[at] = i; peerEnd[at] = end; groupNumber[at] = group; }
                    i = end;
                }
                using var operandMemory = new QueryInputMemory(meter);
                var cachedValues = new SqlValue[n];
                if (plan.Name is "lag" or "lead" or "first_value" or "last_value" or "nth_value")
                    for (var i = 0; i < n; i++)
                    {
                        cachedValues[i] = await Evaluate.ExprAsync(plan.Args[0], meter, row: rows[indices[i]]).ConfigureAwait(false);
                        operandMemory.ReserveValue(cachedValues[i]);
                    }
                var fold = plan.Aggregate is null ? null : new GroupExecution.Fold(plan.Aggregate);
                try
                {
                var previousLow = 0; var previousHigh = 0; var counted = new bool[n];
                var valueCache = new SqlValue[n]; var extraCache = new SqlValue[n][]; var cached = new bool[n];
                var startOffset = Offset(plan.Frame?.Start, plan.Frame?.Mode, meter); var endOffset = Offset(plan.Frame?.End, plan.Frame?.Mode, meter);
                for (var at = 0; at < n; at++)
                {
                    meter.Checkpoint(cancellationToken); meter.Charge(Costs.WindowResult); meter.Guard();
                    var row = rows[indices[at]]; SqlValue result;
                    async ValueTask<SqlValue> Arg(int slot, int position) => await Evaluate.ExprAsync(plan.Args[slot], meter, row: rows[indices[position]]).ConfigureAwait(false);
                    if (plan.Name is "row_number" or "rank" or "dense_rank")
                        result = SqlValue.Integer(plan.Name == "row_number" ? at + 1 : plan.Name == "rank" ? peerStart[at] + 1 : groupNumber[at] + 1);
                    else if (plan.Name is "percent_rank" or "cume_dist")
                        result = SqlValue.Float(plan.Name == "percent_rank" ? n == 1 ? 0 : (double)peerStart[at] / (n - 1) : (double)peerEnd[at] / n, SqlType.F64);
                    else if (plan.Name == "ntile")
                    {
                        var value = await Arg(0, at).ConfigureAwait(false);
                        if (value.IsNull) result = SqlValue.Null(SqlType.I64);
                        else
                        {
                            var buckets = value.AsInteger(); if (buckets <= 0) throw new SqlException(SqlStates.InvalidArgumentForNtile, "ntile requires positive bucket count");
                            var small = n / buckets; var larger = n % buckets; var boundary = (small + 1) * larger;
                            result = SqlValue.Integer(at < boundary ? at / (small + 1) + 1 : small == 0 ? at + 1 : larger + (at - boundary) / small + 1);
                        }
                    }
                    else if (plan.Name is "lag" or "lead")
                    {
                        var offset = plan.Args.Length > 1 ? await Arg(1, at).ConfigureAwait(false) : SqlValue.Integer(1);
                        if (offset.IsNull) result = SqlValue.Null(plan.Type);
                        else
                        {
                            var target = (Int128)at + (plan.Name == "lag" ? -(Int128)offset.AsInteger() : offset.AsInteger());
                            result = target >= 0 && target < n ? cachedValues[(int)target] : plan.Args.Length > 2 ? await Arg(2, at).ConfigureAwait(false) : SqlValue.Null(plan.Type);
                        }
                    }
                    else
                    {
                        int Boundary(WindowBoundSyntax? bound, SqlValue offset, bool end)
                        {
                            if (bound is null) return end ? plan.Order.Length == 0 ? n : peerEnd[at] : 0;
                            if (bound.Kind == "unbounded_preceding") return 0;
                            if (bound.Kind == "unbounded_following") return n;
                            var mode = plan.Frame!.Mode;
                            if (bound.Kind == "current") return mode == "rows" ? at + (end ? 1 : 0) : end ? peerEnd[at] : peerStart[at];
                            if (mode == "range")
                            {
                                var current = set.OrderKeys[indices[at]][0];
                                if (current.IsNull) return end ? peerEnd[at] : peerStart[at];
                                var descending = plan.Order[0].Descending;
                                var subtract = (bound.Kind == "preceding") != descending;
                                double floatingBound = 0; ExactDecimal? decimalBound = null;
                                if (current.Type.IsFloat())
                                {
                                    var amount = Resolve.Cast(offset, SqlType.F64).AsFloat();
                                    floatingBound = subtract ? current.AsFloat() - amount : current.AsFloat() + amount;
                                }
                                else
                                {
                                    var center = Resolve.Cast(current, SqlType.Decimal).AsDecimal(); var amount = Resolve.Cast(offset, SqlType.Decimal).AsDecimal();
                                    decimalBound = center.AddUncapped(subtract ? amount.Negate() : amount);
                                }
                                for (var i = 0; i < n; i++)
                                {
                                    var value = set.OrderKeys[indices[i]][0];
                                    var compare = value.IsNull ? plan.Order[0].NullsFirst ? -1 : 1 : current.Type.IsFloat() ? FloatValue.Compare(value.AsFloat(), floatingBound) : Resolve.Cast(value, SqlType.Decimal).AsDecimal().CompareTo(decimalBound!);
                                    if (!value.IsNull && descending) compare = -compare;
                                    if (end ? compare > 0 : compare >= 0) return i;
                                }
                                return n;
                            }

                            var position = (Int128)(mode == "groups" ? groupNumber[at] : at) + (bound.Kind == "preceding" ? -(Int128)offset.AsInteger() : offset.AsInteger());
                            if (mode == "groups") return position < 0 ? 0 : position >= starts.Count ? n : end ? peerEnd[starts[(int)position]] : starts[(int)position];
                            position += end ? 1 : 0; return position < 0 ? 0 : position > n ? n : (int)position;
                        }
                        var low = Boundary(plan.Frame?.Start, startOffset, false); var high = Boundary(plan.Frame?.End, endOffset, true);
                        if (high < low) high = low;
                        var exclusion = plan.Frame?.Exclude ?? "none";
                        bool Included(int i) => exclusion switch { "current" => i != at, "group" => i < peerStart[at] || i >= peerEnd[at], "ties" => i == at || i < peerStart[at] || i >= peerEnd[at], _ => true };
                        if (plan.Aggregate is { } aggregate)
                        {
                            var reuse = exclusion == "none" && (aggregate.Filter is null || plan.Frame is null) && low >= previousLow && high >= previousHigh && (low == previousLow || aggregate.Name == "count");
                            if (!reuse) { fold?.Release(meter); fold = new(aggregate); previousHigh = low; }
                            if (reuse && aggregate.Name == "count")
                                for (var i = previousLow; i < Math.Min(low, previousHigh); i++) { meter.Charge(Costs.WindowFrameStep); meter.Guard(); if (counted[i]) fold!.Count--; }
                            for (var i = Math.Max(previousHigh, low); i < high; i++)
                            {
                                if (!Included(i)) continue;
                                meter.Charge(Costs.WindowFrameStep); meter.Guard();
                                if (!await QueryExecution.KeepsValueAsync(aggregate.Filter, rows[indices[i]], meter).ConfigureAwait(false)) continue;
                                if (!cached[i])
                                {
                                    valueCache[i] = aggregate.Argument is null ? SqlValue.Integer(1) : await Evaluate.ExprAsync(aggregate.Argument, meter, row: rows[indices[i]]).ConfigureAwait(false);
                                    var operands = new SqlValue[aggregate.Extra?.Length ?? 0];
                                    if (operands.Length != 0) { meter.Charge(Costs.OperatorEval); meter.Guard(); }
                                    for (var e = 0; e < operands.Length; e++) operands[e] = await Evaluate.ExprAsync(aggregate.Extra![e], meter, row: rows[indices[i]]).ConfigureAwait(false);
                                    if (plan.Frame is not null)
                                    {
                                        operandMemory.ReserveValue(valueCache[i]);
                                        foreach (var operand in operands) operandMemory.ReserveValue(operand);
                                    }
                                    extraCache[i] = operands; cached[i] = true;
                                }
                                var value = valueCache[i]; var extra = extraCache[i];
                                counted[i] = !value.IsNull; GroupExecution.Accumulate(aggregate, fold!, value, meter, extra);
                            }
                            previousLow = low; previousHigh = high; result = GroupExecution.Finalize(aggregate, fold!);
                        }
                        else
                        {
                            var nth = plan.Name == "nth_value" ? await Arg(1, at).ConfigureAwait(false) : SqlValue.Integer(1);
                            if (nth.IsNull) result = SqlValue.Null(plan.Type);
                            else
                            {
                                var position = nth.AsInteger(); if (position <= 0) throw new SqlException(SqlStates.InvalidArgumentForNthValue, "nth_value requires positive position");
                                var chosen = -1;
                                for (var i = plan.Name == "last_value" ? high - 1 : low; plan.Name == "last_value" ? i >= low : i < high; i += plan.Name == "last_value" ? -1 : 1)
                                {
                                    if (!Included(i)) continue;
                                    if (--position == 0) { chosen = i; break; }
                                }
                                result = chosen < 0 ? SqlValue.Null(plan.Type) : cachedValues[chosen];
                            }
                        }
                    }
                    var appended = Resolve.Cast(result, plan.Type);
                    rows[indices[at]][stage.InputWidth + spec] = appended;
                }
                }
                finally { fold?.Release(meter); operandMemory.Dispose(); }
                foreach (var index in indices) memory.ReserveValue(rows[index][stage.InputWidth + spec]);
            }
        }
        partitions.Dispose();
        foreach (var row in rows) { meter.Checkpoint(cancellationToken); if (meter.Memory.Enabled) memory.TransferBytes(MemorySize.RowBytes(row)); yield return row; }
    }
    private static SqlValue Offset(WindowBoundSyntax? bound, string? mode, Meter meter)
    {
        if (bound?.Offset is null) return SqlValue.Integer(0);
        var resolved = Resolve.Expr(bound.Offset);
        if (!Evaluate.Constant(resolved)) throw new SqlException(SqlStates.WindowingError, "window frame offset must be constant");
        var value = Evaluate.Expr(resolved, meter);
        if (value.IsNull) throw new SqlException("22004", "window frame offset is null");
        if (mode == "range" ? !value.Type.IsNumeric() : !value.Type.IsInteger()) throw Errors.Type("invalid window frame offset type");
        var negative = value.Type.IsInteger() ? value.AsInteger() < 0 : value.Type.IsFloat() ? value.AsFloat() < 0 || double.IsNaN(value.AsFloat()) : value.AsDecimal().CompareTo(ExactDecimal.FromInt64(0)) < 0;
        if (negative) throw new SqlException("22013", "window frame offset is negative");
        return value;
    }
    private static bool SameKeys(WindowPlan a, WindowPlan b) => a.Partition.All(key => key.Op == "column") && a.Order.All(key => key.Expression.Op == "column") && a.Partition.Length == b.Partition.Length && a.Order.Length == b.Order.Length &&
        a.Partition.Zip(b.Partition).All(pair => Resolve.Equivalent(pair.First, pair.Second)) && a.Order.Zip(b.Order).All(pair =>
            pair.First.Descending == pair.Second.Descending && pair.First.NullsFirst == pair.Second.NullsFirst && Resolve.Equivalent(pair.First.Expression, pair.Second.Expression));
    private static int CompareKeys(SortPlan[] order, SortValue[] a, SortValue[] b)
    {
        for (var i = 0; i < order.Length; i++)
        {
            var compare = SortValue.Compare(a[i], b[i], order[i]);
            if (compare != 0) return compare;
        }
        return 0;
    }
    private static async ValueTask<PartitionSet> Partition(WindowPlan plan, List<SqlValue[]> rows, Meter meter, QueryInputMemory memory)
    {
        var groups = new List<List<int>>(); var lookup = new Dictionary<string, int>(StringComparer.Ordinal); var orderKeys = new SqlValue[rows.Count][];
        var decorations = new SortValue[rows.Count][];
        for (var i = 0; i < rows.Count; i++)
        {
            var keys = new SqlValue[plan.Partition.Length];
            for (var key = 0; key < keys.Length; key++) { keys[key] = await Evaluate.ExprAsync(plan.Partition[key], meter, row: rows[i]).ConfigureAwait(false); GroupExecution.ChargeKey(keys[key], meter); }
            var identity = GroupExecution.Identity(keys);
            if (!lookup.TryGetValue(identity, out var group)) { if (meter.Memory.Enabled) memory.ReserveBytes(MemorySize.EntryBytes(keys)); group = groups.Count; groups.Add([]); lookup.Add(identity, group); }
            groups[group].Add(i); orderKeys[i] = new SqlValue[plan.Order.Length]; decorations[i] = new SortValue[plan.Order.Length];
            for (var key = 0; key < plan.Order.Length; key++)
            {
                var value = await Evaluate.ExprAsync(plan.Order[key].Expression, meter, row: rows[i]).ConfigureAwait(false); orderKeys[i][key] = value;
                decorations[i][key] = CollationRules.Decorate(value, plan.Order[key].Expression, meter);
            }
            memory.ReserveBytes(MemorySize.SortKeyBytes(decorations[i]));
        }
        foreach (var group in groups) group.Sort((a, b) => { var order = CompareKeys(plan.Order, decorations[a], decorations[b]); return order == 0 ? a.CompareTo(b) : order; });
        return new(groups.Select(group => group.ToArray()).ToList(), orderKeys, decorations);
    }
}
