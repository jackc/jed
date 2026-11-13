namespace Jed;

// A fixed number of keyed scratch directories serves every group and aggregate;
// there is no resident descriptor, list, or open file per group.
internal sealed class SpillAggregateInputs(SpillPolicy policy, Meter meter) : IAsyncDisposable
{
    private readonly SpillKeyState inputs = new(policy.Host, policy.WorkMem, meter);
    private readonly SpillKeyState membership = new(policy.Host, policy.WorkMem, meter);
    private static byte[] Key(byte kind, long group, int aggregate, int phase, long ordinal = 0, byte[]? suffix = null)
    {
        var writer = new WireWriter(); writer.Byte(kind); writer.U64((ulong)group); writer.U32((uint)aggregate); writer.U32((uint)phase); writer.U64((ulong)ordinal);
        if (suffix is not null) writer.Bytes(suffix); return writer.ToArray();
    }
    private async ValueTask Append(long group, int aggregate, int phase, SpillFold fold, SqlValue[] values, long bytes, CancellationToken token)
    {
        await inputs.Put(Key(0, group, aggregate, phase, fold.Items), ScratchValues.Encode(values), bytes, token).ConfigureAwait(false);
        fold.Items = checked(fold.Items + 1);
    }
    private async ValueTask<SqlValue[]> Item(long group, int aggregate, int phase, long ordinal, SqlType[] types, CancellationToken token)
    {
        var value = await inputs.Get(Key(0, group, aggregate, phase, ordinal), token).ConfigureAwait(false) ?? throw new InvalidOperationException("missing aggregate scratch input");
        return ScratchValues.Decode(value.Bytes, types);
    }
    internal async ValueTask<bool> Distinct(long group, int aggregate, SqlValue value, SqlValue[] extra, AggregatePlan plan, CancellationToken token)
    {
        if (value.IsNull && plan.Name != "array_agg" && !AggregateJson.IsJson(plan.Name)) return false;
        var identity = Key(1, group, aggregate, 0, suffix: Convert.FromHexString(GroupExecution.Identity(new[] { value }.Concat(extra).ToArray())));
        if (await membership.Get(identity, token).ConfigureAwait(false) is not null) return false;
        var bytes = !value.IsNull || plan.Name == "array_agg" || AggregateJson.IsJson(plan.Name) ? MemorySize.EntryBytes([SqlValue.Integer(group), SqlValue.Integer(aggregate), value]) : 0;
        await membership.Put(identity, [], bytes, token).ConfigureAwait(false); return true;
    }
    private static SqlValue[] Decorated(SqlValue[] values, SortValue[] keys) => values.Concat(keys.Select(key => key.Key is null ? SqlValue.Null(SqlType.Bytea) : SqlValue.Bytes(key.Key))).ToArray();
    private static SortValue Decoration(SqlValue[] row, int value, int key) => new(row[value], row[key].IsNull ? null : row[key].AsBytes());
    internal ValueTask Ordered(long group, int aggregate, SpillFold fold, SqlValue value, SqlValue[] extra, SortValue[] keys, CancellationToken token)
    {
        var row = new[] { value }.Concat(extra).Concat(keys.Select(key => key.Value)).Concat(keys.Select(key => key.Key is null ? SqlValue.Null(SqlType.Bytea) : SqlValue.Bytes(key.Key))).ToArray();
        var bytes = Meter.Add(MemorySchedule.Entry, MemorySize.RowBytes(keys.Select(key => key.Key is null ? key.Value : SqlValue.Bytes(key.Key)).Append(value).ToArray()));
        return Append(group, aggregate, 0, fold, row, bytes, token);
    }
    internal async ValueTask Within(long group, int aggregate, AggregatePlan plan, SpillFold fold, SqlValue[] values, CancellationToken token)
    {
        var hypothetical = AggregateOrdered.Hypothetical(plan.Name);
        if (!hypothetical && values[0].IsNull) return;
        if (plan.Name == "percentile_cont" && values[0].Type != SqlType.Interval) values = [Resolve.Cast(values[0], SqlType.F64)];
        var bytes = Meter.Add(MemorySchedule.Entry, MemorySize.RowBytes(values));
        if (hypothetical)
        {
            var keys = values.Select((value, i) => CollationRules.Decorate(value, plan.WithinOrder![i].Expression, meter)).ToArray();
            values = Decorated(values, keys);
        }
        meter.Guard(); await Append(group, aggregate, 0, fold, values, bytes, token).ConfigureAwait(false);
    }
    private static byte[] Shape(SqlArray array)
    {
        var writer = new WireWriter(); writer.Byte((byte)array.Rank);
        for (var i = 0; i < array.Rank; i++) { writer.U32((uint)array.Dimensions[i]); writer.U32(unchecked((uint)array.LowerBounds[i])); }
        return writer.ToArray();
    }
    internal async ValueTask Add(long group, int aggregate, int phase, AggregatePlan plan, SpillFold fold, SqlValue value, SqlValue[] extra, CancellationToken token)
    {
        if (AggregateJson.IsJson(plan.Name)) { await Json(group, aggregate, phase, plan, fold, value, extra, token).ConfigureAwait(false); return; }
        if (plan.Name == "array_agg")
        {
            if (plan.Argument!.Type.Kind == SqlKind.Array)
            {
                if (value.IsNull) throw new SqlException(SqlStates.NullValueNotAllowed, "cannot accumulate null arrays");
                var array = value.AsArray(); if (array.Rank == 0) throw new SqlException(SqlStates.ArraySubscriptError, "cannot accumulate empty arrays");
                var shape = Shape(array);
                if (fold.ArrayShape.Length != 0 && !fold.ArrayShape.AsSpan().SequenceEqual(shape)) throw new SqlException(SqlStates.ArraySubscriptError, "array dimensions must match");
                fold.ArrayShape = shape;
            }
            fold.Finite.Count++;
            await Append(group, aggregate, phase, fold, [value], Meter.Add(MemorySchedule.Entry, MemorySize.RowBytes([value])), token).ConfigureAwait(false); return;
        }
        if (plan.Name == "string_agg")
        {
            if (value.IsNull) return;
            fold.Finite.Count++;
            await Append(group, aggregate, phase, fold, [value, extra[0]], Meter.Add(MemorySchedule.Entry, MemorySize.RowBytes([value, extra[0]])), token).ConfigureAwait(false); return;
        }
        GroupExecution.Accumulate(plan, fold.Finite, value, meter, extra);
    }
    private async ValueTask Json(long group, int aggregate, int phase, AggregatePlan plan, SpillFold fold, SqlValue value, SqlValue[] extra, CancellationToken token)
    {
        fold.JsonSeen = true;
        var binary = plan.Type == SqlType.Jsonb; var objectAggregate = AggregateJson.Object(plan.Name);
        var image = objectAggregate ? extra[0] : value; var strict = plan.Name.EndsWith("_strict", StringComparison.Ordinal);
        if (!objectAggregate && strict && image.IsNull) return;
        meter.Charge(Costs.GeneratedRow + (TemporalRender.ZoneCount(value) + (objectAggregate ? TemporalRender.ZoneCount(image) : 0)) * Costs.Timezone); meter.Guard();
        string? key = null;
        if (objectAggregate)
        {
            key = JsonImage.Key(value, meter.Temporal, binary);
            if (plan.Name.Contains("_unique", StringComparison.Ordinal))
            {
                var identity = Key(2, group, aggregate, phase, suffix: TextEncoding.Utf8.GetBytes(key));
                if (await membership.Get(identity, token).ConfigureAwait(false) is not null) throw new SqlException("22030", "duplicate JSON object key value");
                await membership.Put(identity, [], MemorySize.EntryBytes([SqlValue.Integer(group), SqlValue.Integer(aggregate), SqlValue.Text(key)]), token).ConfigureAwait(false);
            }
            if (strict && image.IsNull) return;
        }
        var rendered = binary ? SqlValue.Jsonb(JsonImage.Node(image, meter.Temporal)) :
            SqlValue.Text((!objectAggregate && fold.Items != 0 && !image.IsNull && image.Type.Kind is SqlKind.Array or SqlKind.Composite ? "\n " : "") + JsonImage.Text(image, meter.Temporal));
        var node = binary ? rendered : SqlValue.Jsonb(JsonImage.Node(image, meter.Temporal));
        var bytes = Meter.Add(MemorySchedule.Entry, MemorySize.RowBytes(objectAggregate ? [SqlValue.Text(key!), node] : [node]));
        await Append(group, aggregate, phase, fold, [key is null ? SqlValue.Null(SqlType.Text) : SqlValue.Text(key), rendered], bytes, token).ConfigureAwait(false);
    }
    internal async ValueTask<SqlValue> Result(long group, int aggregate, AggregatePlan plan, SpillFold fold, SqlValue[] direct, CancellationToken token)
    {
        if (plan.WithinOrder is not null) return await WithinResult(group, aggregate, plan, fold, direct, token).ConfigureAwait(false);
        var phase = 0;
        if (plan.Order is { Length: > 0 } order)
        {
            var values = 1 + (plan.Extra?.Length ?? 0); var keyStart = values + order.Length;
            var types = new[] { plan.Argument?.Type ?? SqlType.I64 }.Concat(plan.Extra?.Select(value => value.Type) ?? []).Concat(order.Select(key => key.Expression.Type)).Concat(order.Select(_ => SqlType.Bytea)).ToArray();
            int Compare(SqlValue[] a, SqlValue[] b)
            {
                for (var k = 0; k < order.Length; k++) { var compared = SortValue.Compare(Decoration(a, values + k, keyStart + k), Decoration(b, values + k, keyStart + k), order[k]); if (compared != 0) return compared; }
                return 0;
            }
            await using var sorter = new ExternalSorter(policy.Host, policy.WorkMem, meter, Compare);
            for (long i = 0; i < fold.Items; i++) await sorter.Add(await Item(group, aggregate, 0, i, types, token).ConfigureAwait(false), token).ConfigureAwait(false);
            fold = new(plan); phase = 1;
            await foreach (var input in sorter.Read(token).ConfigureAwait(false)) await Add(group, aggregate, phase, plan, fold, input[0], input.Skip(1).Take(values - 1).ToArray(), token).ConfigureAwait(false);
        }
        if (AggregateJson.IsJson(plan.Name))
        {
            if (!fold.JsonSeen) return SqlValue.Null(plan.Type);
            var binary = plan.Type == SqlType.Jsonb; var objectAggregate = AggregateJson.Object(plan.Name);
            var elements = new List<SqlJsonNode>(); var members = new List<SqlJsonMember>(); var text = new System.Text.StringBuilder(objectAggregate ? "{ " : "[");
            for (long i = 0; i < fold.Items; i++)
            {
                var item = await Item(group, aggregate, phase, i, [SqlType.Text, binary ? SqlType.Jsonb : SqlType.Text], token).ConfigureAwait(false);
                if (binary) { if (objectAggregate) members.Add(new(item[0].AsText(), item[1].AsJsonb())); else elements.Add(item[1].AsJsonb()); }
                else { if (i != 0) text.Append(", "); if (objectAggregate) text.Append(JsonImage.Quote(item[0].AsText())).Append(" : "); text.Append(item[1].AsText()); }
            }
            if (binary) return SqlValue.Jsonb(objectAggregate ? SqlJsonNode.Object(members) : SqlJsonNode.Array(elements));
            return SqlValue.Json(text.Append(objectAggregate ? " }" : "]").ToString());
        }
        if (plan.Name == "array_agg")
        {
            if (fold.Items == 0) return SqlValue.Null(plan.Type);
            var values = new List<SqlValue>(); SqlArray? first = null;
            for (long i = 0; i < fold.Items; i++)
            {
                var value = (await Item(group, aggregate, phase, i, [plan.Argument!.Type], token).ConfigureAwait(false))[0];
                if (plan.Argument.Type.Kind == SqlKind.Array) { first ??= value.AsArray(); values.AddRange(value.AsArray().Values); }
                else values.Add(value.IsNull ? SqlValue.Null(plan.Type.Element!) : value);
            }
            return SqlValue.Array(first is null ? new(plan.Type.Element!, values) : new(first.ElementType, values, new[] { checked((int)fold.Items) }.Concat(first.Dimensions), new[] { 1 }.Concat(first.LowerBounds)));
        }
        if (plan.Name == "string_agg")
        {
            if (fold.Items == 0) return SqlValue.Null(plan.Type);
            var text = new System.Text.StringBuilder(); var bytes = new List<byte>();
            for (long i = 0; i < fold.Items; i++)
            {
                var item = await Item(group, aggregate, phase, i, [plan.Type, plan.Type], token).ConfigureAwait(false);
                if (plan.Type == SqlType.Text) { if (i != 0 && !item[1].IsNull) text.Append(item[1].AsText()); text.Append(item[0].AsText()); }
                else { if (i != 0 && !item[1].IsNull) bytes.AddRange(item[1].AsBytes()); bytes.AddRange(item[0].AsBytes()); }
            }
            return plan.Type == SqlType.Text ? SqlValue.Text(text.ToString()) : SqlValue.Bytes(bytes.ToArray());
        }
        return GroupExecution.Finalize(plan, fold.Finite);
    }
    private async ValueTask<SqlValue> WithinResult(long group, int aggregate, AggregatePlan plan, SpillFold fold, SqlValue[] direct, CancellationToken token)
    {
        var order = plan.WithinOrder!; var width = order.Length;
        var types = order.Select(key => key.Expression.Type).ToArray();
        if (plan.Name == "percentile_cont" && types[0] != SqlType.Interval) types = [SqlType.F64];
        int Compare(SqlValue[] a, SqlValue[] b)
        {
            for (var i = 0; i < width; i++) { var compared = SortValue.Compare(Decoration(a, i, width + i), Decoration(b, i, width + i), order[i]); if (compared != 0) return compared; }
            return 0;
        }
        if (AggregateOrdered.Hypothetical(plan.Name))
        {
            var storedTypes = types.Concat(types.Select(_ => SqlType.Bytea)).ToArray();
            var target = Decorated(direct, direct.Select((value, i) => CollationRules.Decorate(value, order[i].Expression)).ToArray());
            long before = 0, inclusive = 0, distinct = 0;
            for (long i = 0; i < fold.Items; i++)
            {
                var item = await Item(group, aggregate, 0, i, storedTypes, token).ConfigureAwait(false); var comparison = Compare(item, target);
                if (comparison <= 0) inclusive++;
                if (comparison >= 0) continue;
                before++;
                if (plan.Name == "dense_rank")
                {
                    var identity = Key(3, group, aggregate, 0, suffix: Convert.FromHexString(GroupExecution.Identity(item.Take(width).ToArray())));
                    if (await membership.Get(identity, token).ConfigureAwait(false) is null)
                    { await membership.Put(identity, [], MemorySize.EntryBytes(item.Take(width).ToArray()), token).ConfigureAwait(false); distinct++; }
                }
            }
            return plan.Name switch
            {
                "rank" => SqlValue.Integer(before + 1), "dense_rank" => SqlValue.Integer(distinct + 1),
                "percent_rank" => SqlValue.Float(fold.Items == 0 ? 0 : (double)before / fold.Items, SqlType.F64),
                _ => SqlValue.Float((double)(inclusive + 1) / (fold.Items + 1), SqlType.F64)
            };
        }
        await using var sorter = new ExternalSorter(policy.Host, policy.WorkMem, meter, Compare);
        for (long i = 0; i < fold.Items; i++)
        {
            var values = await Item(group, aggregate, 0, i, types, token).ConfigureAwait(false);
            var keys = values.Select((value, k) => CollationRules.Decorate(value, order[k].Expression, meter)).ToArray();
            await sorter.Add(Decorated(values, keys), token).ConfigureAwait(false);
        }
        if (plan.Name == "mode")
        {
            SqlValue[]? previous = null; var best = SqlValue.Null(plan.Type); long run = 0, bestRun = 0;
            await foreach (var row in sorter.Read(token).ConfigureAwait(false))
            {
                if (previous is null || Compare(previous, row) != 0) { previous = row; run = 0; }
                run++; if (run > bestRun) { bestRun = run; best = previous[0]; }
            }
            return best;
        }
        await using var sorted = new SpillRowSpool(policy.Host, policy.WorkMem, meter);
        await sorted.ForceDisk(token).ConfigureAwait(false);
        await foreach (var row in sorter.Read(token).ConfigureAwait(false)) await sorted.Append([row[0]], token).ConfigureAwait(false);
        await sorter.DisposeAsync().ConfigureAwait(false);
        var fraction = direct[0]; if (fraction.IsNull) return SqlValue.Null(plan.Type);
        static double? Fraction(SqlValue value)
        {
            if (value.IsNull) return null;
            var p = Resolve.Cast(value, SqlType.F64).AsFloat(); if (double.IsNaN(p) || p < 0 || p > 1) throw Errors.Overflow(); return p;
        }
        async ValueTask<SqlValue> At(double? p, SqlType type)
        {
            if (p is null || fold.Items == 0) return SqlValue.Null(type);
            var position = plan.Name == "percentile_disc" ? Math.Max(0, Math.Ceiling(p.Value * fold.Items) - 1) : p.Value * (fold.Items - 1);
            var lo = (long)Math.Floor(position); var hi = (long)Math.Ceiling(position); long ordinal = 0;
            var left = SqlValue.Null(type); var right = left;
            await foreach (var row in sorted.Read(token).ConfigureAwait(false))
            { if (ordinal == lo) left = row[0]; if (ordinal++ == hi) { right = row[0]; break; } }
            if (lo == hi) return left;
            var proportion = position - lo;
            if (left.Type == SqlType.Interval) return SqlValue.Interval(AggregateOrdered.IntervalLerp(left.AsInterval(), right.AsInterval(), proportion));
            var delta = right.AsFloat() - left.AsFloat(); var scaled = proportion * delta;
            return SqlValue.Float(left.AsFloat() + scaled, SqlType.F64);
        }
        if (fraction.Type.Kind == SqlKind.Array)
        {
            var array = fraction.AsArray(); var proportions = array.Values.Select(Fraction).ToArray();
            if (fold.Items == 0) return SqlValue.Null(plan.Type);
            var values = new SqlValue[proportions.Length];
            for (var i = 0; i < values.Length; i++) values[i] = await At(proportions[i], plan.Type.Element!).ConfigureAwait(false);
            return SqlValue.Array(new(plan.Type.Element!, values, array.Dimensions, array.LowerBounds));
        }
        return await At(Fraction(fraction), plan.Type).ConfigureAwait(false);
    }
    public async ValueTask DisposeAsync()
    { try { await inputs.DisposeAsync().ConfigureAwait(false); } finally { await membership.DisposeAsync().ConfigureAwait(false); } }
}
