using System.Runtime.CompilerServices;

namespace Jed;

// Input and state stay in original fold order. Only finalization replays the
// first-occurrence directory, once for each grouping set.
internal static class SpillGroup
{
    private sealed record State(SqlValue[] Keys, SpillFold[] Folds, int Set);
    private static State Empty(GroupPlan plan, SqlValue[] keys, int set) => new(keys, plan.Aggregates.Select(a => new SpillFold(a)).ToArray(), set);
    private static long Size(State state, SqlValue[] selected) => state.Folds.Aggregate(
        Meter.Add(MemorySize.EntryBytes(selected), MemorySchedule.Value), (bytes, fold) => Meter.Add(bytes, fold.LogicalBytes()));
    private static byte[] Encode(State state)
    {
        var writer = new WireWriter(); writer.U32((uint)state.Set);
        foreach (var key in state.Keys) ScratchValues.Write(writer, key);
        foreach (var fold in state.Folds) fold.Write(writer);
        return writer.ToArray();
    }
    private static State Decode(byte[] bytes, GroupPlan plan)
    {
        var reader = new WireReader(bytes); var set = checked((int)reader.U32());
        var keys = new SqlValue[plan.Keys.Length];
        for (var i = 0; i < keys.Length; i++) keys[i] = ScratchValues.Read(ref reader, plan.Keys[i].Type);
        var state = Empty(plan, keys, set);
        foreach (var fold in state.Folds) fold.Read(ref reader);
        reader.Finish(); return state;
    }
    private static byte[] Key(int set, SqlValue[] values)
    {
        var writer = new WireWriter(); writer.U32((uint)set); writer.Bytes(Convert.FromHexString(GroupExecution.Identity(values))); return writer.ToArray();
    }
    private static async IAsyncEnumerable<SqlValue[]> Transferred(IAsyncEnumerable<SqlValue[]> source, Meter meter, [EnumeratorCancellation] CancellationToken token)
    {
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        { try { yield return row; } finally { meter.Memory.Release(MemorySize.RowBytes(row)); } }
    }
    internal static async IAsyncEnumerable<SqlValue[]> Run(GroupPlan plan, IAsyncEnumerable<SqlValue[]> source,
        Resolved? filter, SpillPolicy policy, Meter meter, bool sourceMemory, IReadOnlySet<int>? touched, bool lane,
        ExplainNode? filterObservation, [EnumeratorCancellation] CancellationToken token)
    {
        await using var inputs = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var output = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await using var groups = new SpillGroupStore(policy, meter, lane);
        await using var collections = new SpillAggregateInputs(policy, meter);
        if (lane && sourceMemory) source = Transferred(source, meter, token);
        if (!lane)
        {
            await foreach (var item in source.WithCancellation(token).ConfigureAwait(false))
            {
                var row = new SqlValue[item.Length];
                for (var i = 0; i < row.Length; i++) row[i] = touched is not null && !touched.Contains(i) ? SqlValue.Null(item[i].Type) : await item[i].ResolveAsync(token).ConfigureAwait(false);
                if (sourceMemory) meter.Memory.Release(MemorySize.RowBytes(item));
                if (await QueryExecution.KeepsAsync(filter, row, meter, filterObservation).ConfigureAwait(false)) await inputs.Append(row, token).ConfigureAwait(false);
            }
            source = inputs.Read(token); filter = null;
        }
        var sets = plan.Sets ?? [Enumerable.Range(0, plan.Keys.Length).ToArray()];
        for (var set = 0; set < sets.Length; set++)
            if (sets[set].Length == 0)
            {
                var keys = plan.Keys.Select(key => SqlValue.Null(key.Type)).ToArray();
                var empty = Empty(plan, keys, set);
                await groups.Put(Key(set, []), Encode(empty), [], Size(empty, []), token).ConfigureAwait(false);
            }
        await foreach (var row in source.WithCancellation(token).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(filter, row, meter, filterObservation).ConfigureAwait(false)) continue;
            var keys = new SqlValue[plan.Keys.Length];
            for (var i = 0; i < keys.Length; i++) keys[i] = await Evaluate.ExprAsync(plan.Keys[i], meter, row: row).ConfigureAwait(false);
            for (var set = 0; set < sets.Length; set++)
            {
                var selected = sets[set].Select(slot => keys[slot]).ToArray();
                foreach (var key in selected) GroupExecution.ChargeKey(key, meter);
                var identity = Key(set, selected); var held = await groups.Get(identity, token).ConfigureAwait(false);
                State group; long ordinal;
                if (held is null)
                {
                    var outputKeys = plan.Keys.Select(key => SqlValue.Null(key.Type)).ToArray();
                    foreach (var slot in sets[set]) outputKeys[slot] = keys[slot];
                    group = Empty(plan, outputKeys, set); ordinal = await groups.Put(identity, Encode(group), selected, Size(group, selected), token).ConfigureAwait(false);
                }
                else { group = Decode(held.Bytes, plan); ordinal = held.Ordinal; }
                for (var i = 0; i < plan.Aggregates.Length; i++)
                {
                    var aggregate = plan.Aggregates[i]; var fold = group.Folds[i];
                    if (!await QueryExecution.KeepsValueAsync(aggregate.Filter, row, meter).ConfigureAwait(false)) continue;
                    meter.Charge(Costs.AggregateAccumulate); meter.Guard();
                    if (aggregate.WithinOrder is { } within)
                    {
                        var tuple = new SqlValue[within.Length];
                        for (var k = 0; k < tuple.Length; k++) tuple[k] = await Evaluate.ExprAsync(within[k].Expression, meter, row: row).ConfigureAwait(false);
                        await collections.Within(ordinal, i, aggregate, fold, tuple, token).ConfigureAwait(false);
                        continue;
                    }
                    var value = aggregate.Argument is null ? SqlValue.Integer(1) : await Evaluate.ExprAsync(aggregate.Argument, meter, row: row).ConfigureAwait(false);
                    if (aggregate.Extra is { Length: > 0 }) { meter.Charge(Costs.OperatorEval); meter.Guard(); }
                    var extra = new SqlValue[aggregate.Extra?.Length ?? 0];
                    for (var e = 0; e < extra.Length; e++) extra[e] = await Evaluate.ExprAsync(aggregate.Extra![e], meter, row: row).ConfigureAwait(false);
                    var order = aggregate.Order ?? []; var orderKeys = new SortValue[order.Length];
                    for (var k = 0; k < order.Length; k++) orderKeys[k] = CollationRules.Decorate(await Evaluate.ExprAsync(order[k].Expression, meter, row: row).ConfigureAwait(false), order[k].Expression, meter);
                    if (aggregate.Distinct)
                    {
                        GroupExecution.ChargeKey(value, meter); foreach (var item in extra) GroupExecution.ChargeKey(item, meter);
                        if (!await collections.Distinct(ordinal, i, value, extra, aggregate, token).ConfigureAwait(false)) continue;
                    }
                    if (order.Length != 0) await collections.Ordered(ordinal, i, fold, value, extra, orderKeys, token).ConfigureAwait(false);
                    else await collections.Add(ordinal, i, 0, aggregate, fold, value, extra, token).ConfigureAwait(false);
                }
                await groups.Put(identity, Encode(group), selected, Size(group, selected), token).ConfigureAwait(false);
            }
            await groups.EndRow(token).ConfigureAwait(false);
        }
        await inputs.DisposeAsync().ConfigureAwait(false);
        for (var set = 0; set < sets.Length; set++)
            await foreach (var entry in groups.Read(token).ConfigureAwait(false))
            {
                var group = Decode(entry.Value.Bytes, plan); if (group.Set != set) continue;
                var grouping = plan.GroupingColumns ?? []; var aggregateStart = plan.Keys.Length + grouping.Length;
                var row = new SqlValue[aggregateStart + plan.Aggregates.Length]; group.Keys.CopyTo(row, 0);
                for (var i = 0; i < grouping.Length; i++)
                {
                    var mask = 0; foreach (var column in grouping[i]) mask = (mask << 1) | (sets[set].Contains(column) ? 0 : 1);
                    row[plan.Keys.Length + i] = SqlValue.Integer(mask, SqlType.I32);
                }
                for (var i = 0; i < plan.Aggregates.Length; i++)
                {
                    var aggregate = plan.Aggregates[i]; var direct = new SqlValue[aggregate.Direct?.Length ?? 0];
                    using (meter.SuppressCost()) for (var d = 0; d < direct.Length; d++) direct[d] = await Evaluate.ExprAsync(aggregate.Direct![d], meter, row: row).ConfigureAwait(false);
                    row[aggregateStart + i] = await collections.Result(entry.Value.Ordinal, i, aggregate, group.Folds[i], direct, token).ConfigureAwait(false);
                }
                if (await QueryExecution.KeepsAsync(plan.Having, row, meter).ConfigureAwait(false)) await output.Append(row, token).ConfigureAwait(false);
            }
        await groups.DisposeAsync().ConfigureAwait(false); await collections.DisposeAsync().ConfigureAwait(false);
        await foreach (var row in output.Read(token).ConfigureAwait(false)) yield return row;
    }
}
