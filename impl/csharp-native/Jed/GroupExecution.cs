using System.Runtime.CompilerServices;

namespace Jed;

internal static class GroupExecution
{
    internal sealed record Input(SqlValue Value, SqlValue[] Extra, SortValue[] Keys, int Ordinal);
    internal sealed class Fold(AggregatePlan plan)
    {
        internal readonly AggregateMoments? Moments = AggregateMoments.Single(plan.Name) || AggregateMoments.Pair(plan.Name) ? new() : null;
        internal readonly AggregateJson? Json = AggregateJson.IsJson(plan.Name) ? new() : null;
        internal readonly AggregateOrdered? Ordered = plan.WithinOrder is null ? null : new();
        internal readonly AggregateFloat? Floating = plan.Name is "sum" or "avg" && plan.Type.IsFloat() ? new() : null;
        private List<Input>? buffered;
        private List<SqlValue>? collected;
        private System.Text.StringBuilder? text;
        private List<byte>? bytes;
        internal List<Input> Buffered => buffered ??= [];
        internal List<SqlValue> Collected => collected ??= [];
        internal System.Text.StringBuilder Text => text ??= new();
        internal List<byte> Bytes => bytes ??= [];
        internal long Count;
        internal SqlValue Value = SqlValue.Null(plan.Type);
        internal AggregateIntegerSum? IntegerSum = plan.Name is "sum" or "avg" && plan.Type == SqlType.Decimal && plan.Argument?.Type.IsInteger() == true ? new() : null;
        private ExactDecimal decimalSum = ExactDecimal.Zero;
        internal ExactDecimal Sum
        {
            get => IntegerSum?.Result ?? decimalSum;
            // Scratch restoration can resume with the ordinary decimal fold.
            set { decimalSum = value; IntegerSum = null; }
        }
        internal long SumLogicalBytes => IntegerSum?.LogicalBytes ?? MemorySize.ValueBytes(SqlValue.StoredDecimal(decimalSum));
        private HashSet<byte[]>? seen;
        internal HashSet<byte[]> Seen => seen ??= new(ByteKeyComparer.Instance);
        internal long MemoryBytes;
        internal bool StorageManaged;
        internal void Retain(long bytes, Meter meter) { if (StorageManaged) return; meter.Memory.Reserve(bytes); MemoryBytes = Meter.Add(MemoryBytes, bytes); }
        internal void Release(Meter meter) { meter.Memory.Release(MemoryBytes); MemoryBytes = 0; Ordered?.Release(meter); }
    }
    private sealed class Group(SqlValue[] keys, Fold[] folds, int set, long bytes)
    {
        internal SqlValue[] Keys = keys;
        internal Fold[] Folds = folds;
        internal int Set = set;
        internal long MemoryBytes = bytes;
        internal void Release(Meter meter)
        { meter.Memory.Release(MemoryBytes); MemoryBytes = 0; foreach (var fold in Folds) fold.Release(meter); }
    }

    internal static bool Lane(SelectPlan plan) => plan.Group is { Aggregates.Length: > 0 } group && !plan.Empty && plan.Access?.Kind is null or AccessKind.Full or AccessKind.Primary && !plan.Distinct && plan.DistinctOnCount == 0 && plan.Windows is null && group.Having is null &&
        group.Sets is not { Length: > 1 } && group.GroupingColumns is not { Length: > 0 } &&
        (group.Keys.Length == 0 || group.Keys is [{ Op: "column" } key] && key.Type.IsInteger()) &&
        group.Aggregates.All(aggregate => aggregate.Name is "count" or "sum" or "avg" or "min" or "max" && !aggregate.Distinct &&
            aggregate.Filter is null && aggregate.Order is not { Length: > 0 } && aggregate.WithinOrder is null &&
            (aggregate.Name is not ("sum" or "avg") || aggregate.Argument?.Type.Kind is SqlKind.I16 or SqlKind.I32 or SqlKind.I64 or SqlKind.Decimal or SqlKind.F32 or SqlKind.F64) &&
            (aggregate.Argument is null || AggregateLanePlan.DiscardSafe(aggregate.Argument)));
    private static async IAsyncEnumerable<SqlValue[]> Buffered(IEnumerable<SqlValue[]> rows)
    { await ValueTask.CompletedTask.ConfigureAwait(false); foreach (var row in rows) yield return row; }
    internal static async IAsyncEnumerable<SqlValue[]> Run(GroupPlan plan, IAsyncEnumerable<SqlValue[]> source, Resolved? filter,
        Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken = default, bool sourceMemory = false, IReadOnlySet<int>? touched = null, bool lane = false, ExplainNode? filterObservation = null, SpillPolicy? spill = null)
    {
        if (spill is not null)
        {
            await foreach (var row in SpillGroup.Run(plan, source, filter, spill, meter, sourceMemory, touched, lane, filterObservation, cancellationToken).ConfigureAwait(false)) yield return row;
            yield break;
        }
        using var inputMemory = new QueryInputMemory(meter);
        using var outputMemory = new QueryInputMemory(meter);
        var output = new List<SqlValue[]>();
        if (!lane)
        {
            var inputs = new List<SqlValue[]>();
            await foreach (var item in source.WithCancellation(cancellationToken).ConfigureAwait(false))
            {
                var row = await inputMemory.TakeAsync(item, sourceMemory, touched).ConfigureAwait(false);
                inputs.Add(row);
            }
            var retained = 0;
            for (var index = 0; index < inputs.Count; index++)
            {
                var row = inputs[index];
                if (await QueryExecution.KeepsAsync(filter, row, meter, filterObservation).ConfigureAwait(false)) inputs[retained++] = row;
                else inputMemory.Release(row);
            }
            if (retained < inputs.Count) inputs.RemoveRange(retained, inputs.Count - retained);
            source = Buffered(inputs); filter = null;
        }
        if (lane && filter is not null)
        {
            source = AggregateLanePlan.Filtered(source, filter, meter, cancellationToken,
                !meter.HasCostCeiling && plan.Aggregates.All(aggregate => aggregate.Argument is null or { Op: "column" }), filterObservation);
            filter = null;
        }
        var sets = plan.Sets ?? [Enumerable.Range(0, plan.Keys.Length).ToArray()];
        var identitySets = sets.Select(set => set.Length == plan.Keys.Length && set.Where((slot, index) => slot != index).Any() == false).ToArray();
        var groups = sets.Select(_ => new List<Group>()).ToArray();
        var positions = sets.Select(_ => new Dictionary<byte[], int>(ByteKeyComparer.Instance)).ToArray();
        Group Make(SqlValue[] keys, int set)
        {
            var bytes = meter.Memory.Enabled ? MemorySize.EntryBytes(sets[set].Select(slot => keys[slot]).ToArray()) : 0;
            meter.Memory.Reserve(bytes);
            return new(keys, plan.Aggregates.Select(a => new Fold(a)).ToArray(), set, bytes);
        }
        try
        {
        for (var set = 0; set < sets.Length; set++)
            if (sets[set].Length == 0) { groups[set].Add(Make(plan.Keys.Select(key => SqlValue.Null(key.Type)).ToArray(), set)); positions[set].Add(Array.Empty<byte>(), 0); }
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(filter, row, meter, filterObservation).ConfigureAwait(false)) continue;
            var keys = plan.Keys.Length == 0 ? Array.Empty<SqlValue>() : new SqlValue[plan.Keys.Length];
            for (var i = 0; i < keys.Length; i++) keys[i] = await Evaluate.ExprAsync(plan.Keys[i], meter, row: row).ConfigureAwait(false);
            for (var set = 0; set < sets.Length; set++)
            {
                // Keys belong to this row and are never mutated. New groups
                // retain their separate outputKeys array below.
                var selected = identitySets[set] ? keys : sets[set].Length == 0 ? Array.Empty<SqlValue>() : sets[set].Select(slot => keys[slot]).ToArray();
                foreach (var key in selected) ChargeKey(key, meter);
                var identity = EncodedIdentity(selected);
                if (!positions[set].TryGetValue(identity, out var at))
                {
                    var outputKeys = plan.Keys.Select(key => SqlValue.Null(key.Type)).ToArray();
                    foreach (var slot in sets[set]) outputKeys[slot] = keys[slot];
                    at = groups[set].Count; positions[set].Add(identity, at); groups[set].Add(Make(outputKeys, set));
                }
                var group = groups[set][at];
                for (var i = 0; i < plan.Aggregates.Length; i++)
                {
                    var aggregate = plan.Aggregates[i]; var fold = group.Folds[i];
                    if (!await QueryExecution.KeepsValueAsync(aggregate.Filter, row, meter).ConfigureAwait(false)) continue;
                    meter.Charge(Costs.AggregateAccumulate); meter.Guard();
                    if (aggregate.WithinOrder is { } within)
                    {
                        var tuple = new SqlValue[within.Length];
                        for (var k = 0; k < tuple.Length; k++) tuple[k] = await Evaluate.ExprAsync(within[k].Expression, meter, row: row).ConfigureAwait(false);
                        fold.Ordered!.Add(aggregate, tuple, meter);
                        continue;
                    }
                    var value = aggregate.Argument is null ? SqlValue.Integer(1) : await Evaluate.ExprAsync(aggregate.Argument, meter, row: row).ConfigureAwait(false);
                    if (aggregate.Extra is { Length: > 0 }) { meter.Charge(Costs.OperatorEval); meter.Guard(); }
                    var extra = aggregate.Extra is { Length: > 0 } extraExpressions ? new SqlValue[extraExpressions.Length] : Array.Empty<SqlValue>();
                    for (var e = 0; e < extra.Length; e++) extra[e] = await Evaluate.ExprAsync(aggregate.Extra![e], meter, row: row).ConfigureAwait(false);
                    var order = aggregate.Order ?? []; var orderKeys = order.Length == 0 ? Array.Empty<SortValue>() : new SortValue[order.Length];
                    for (var k = 0; k < order.Length; k++)
                    {
                        var keyValue = await Evaluate.ExprAsync(order[k].Expression, meter, row: row).ConfigureAwait(false);
                        orderKeys[k] = CollationRules.Decorate(keyValue, order[k].Expression, meter);
                    }
                    if (aggregate.Distinct)
                    {
                        ChargeKey(value, meter); foreach (var item in extra) ChargeKey(item, meter);
                        if (!fold.Seen.Add(EncodedIdentity(value, extra))) continue;
                        if (meter.Memory.Enabled && (!value.IsNull || aggregate.Name == "array_agg" || AggregateJson.IsJson(aggregate.Name)))
                            fold.Retain(MemorySize.EntryBytes([value]), meter);
                    }
                    if (order.Length != 0)
                    {
                        fold.Buffered.Add(new(value, extra, orderKeys, fold.Buffered.Count));
                        if (meter.Memory.Enabled) fold.Retain(MemorySize.RowBytes(orderKeys.Select(key => key.Key is null ? key.Value : SqlValue.Bytes(key.Key)).Append(value).ToArray()), meter);
                    }
                    else Accumulate(aggregate, fold, value, meter, extra);
                }
            }
        }
        foreach (var group in groups.SelectMany(items => items))
        {
            var grouping = plan.GroupingColumns ?? [];
            var aggregateStart = plan.Keys.Length + grouping.Length;
            var row = new SqlValue[aggregateStart + plan.Aggregates.Length]; group.Keys.CopyTo(row, 0);
            for (var i = 0; i < grouping.Length; i++)
            {
                var mask = 0;
                foreach (var column in grouping[i]) mask = (mask << 1) | (sets[group.Set].Contains(column) ? 0 : 1);
                row[plan.Keys.Length + i] = SqlValue.Integer(mask, SqlType.I32);
            }
            for (var i = 0; i < plan.Aggregates.Length; i++)
            {
                var aggregate = plan.Aggregates[i]; var fold = group.Folds[i];
                if (fold.Ordered is { } ordered)
                {
                    var direct = new SqlValue[aggregate.Direct!.Length];
                    using (meter.SuppressCost())
                        for (var d = 0; d < direct.Length; d++) direct[d] = await Evaluate.ExprAsync(aggregate.Direct[d], meter, row: row).ConfigureAwait(false);
                    row[aggregateStart + i] = ordered.Result(aggregate, direct, meter);
                    continue;
                }
                if (aggregate.Order is { Length: > 0 } order)
                {
                    fold.Buffered.Sort((a, b) => CompareInput(order, a, b));
                    foreach (var input in fold.Buffered) Accumulate(aggregate, fold, input.Value, meter, input.Extra);
                }
                row[aggregateStart + i] = Finalize(aggregate, fold);

            }
            group.Release(meter);
            if (!lane) outputMemory.Reserve(row);
            if (await QueryExecution.KeepsAsync(plan.Having, row, meter).ConfigureAwait(false)) output.Add(row);
            else outputMemory.Release(row);
        }
        if (lane) foreach (var row in output) outputMemory.Reserve(row);
        inputMemory.Dispose();
        foreach (var row in output) { outputMemory.Transfer(row); yield return row; }
        }
        finally { foreach (var group in groups.SelectMany(items => items)) group.Release(meter); }
    }
    internal static void Accumulate(AggregatePlan aggregate, Fold fold, SqlValue value, Meter meter, SqlValue[]? extra = null)
    {
        if (fold.Moments is { } moments) { moments.Add(aggregate, value, extra ?? [], meter); return; }
        if (fold.Json is { } json)
        {
            json.Add(aggregate, value, extra ?? [], meter);
            if (meter.Memory.Enabled && !(aggregate.Name.EndsWith("_strict", StringComparison.Ordinal) && (AggregateJson.Object(aggregate.Name) ? extra![0] : value).IsNull))
                fold.Retain(Meter.Add(MemorySize.ValueBytes(value), AggregateJson.Object(aggregate.Name) ? MemorySize.ValueBytes(extra![0]) : 0), meter);
            return;
        }
        if (aggregate.Name == "array_agg")
        {
            if (aggregate.Argument!.Type.Kind == SqlKind.Array)
            {
                if (value.IsNull) throw new SqlException(SqlStates.NullValueNotAllowed, "cannot accumulate null arrays");
                var array = value.AsArray(); if (array.Rank == 0) throw new SqlException(SqlStates.ArraySubscriptError, "cannot accumulate empty arrays");
                if (fold.Collected.Count != 0)
                {
                    var first = fold.Collected[0].AsArray();
                    if (!array.Dimensions.SequenceEqual(first.Dimensions) || !array.LowerBounds.SequenceEqual(first.LowerBounds)) throw new SqlException(SqlStates.ArraySubscriptError, "array dimensions must match");
                }
            }
            fold.Collected.Add(value); fold.Count++;
            if (meter.Memory.Enabled) fold.Retain(MemorySize.ValueBytes(value), meter);
            return;
        }
        if (value.IsNull) return;
        fold.Count = checked(fold.Count + 1);
        switch (aggregate.Name)
        {
            case "bit_and": case "bit_or": case "bit_xor":
                var operand = value.AsInteger();
                var beforeBits = fold.Count == 1 ? operand : fold.Value.AsInteger();
                fold.Value = SqlValue.Integer(fold.Count == 1 ? operand : aggregate.Name == "bit_and" ? beforeBits & operand : aggregate.Name == "bit_or" ? beforeBits | operand : beforeBits ^ operand, aggregate.Type); break;
            case "string_agg":
                if (aggregate.Type == SqlType.Text)
                {
                    if (fold.Count > 1 && !extra![0].IsNull) fold.Text.Append(extra[0].AsText());
                    fold.Text.Append(value.AsText());
                }
                else
                {
                    if (fold.Count > 1 && !extra![0].IsNull) fold.Bytes.AddRange(extra[0].AsBytes());
                    fold.Bytes.AddRange(value.AsBytes());
                }
                if (meter.Memory.Enabled) fold.Retain(Meter.Add(MemorySize.ValueBytes(value), MemorySize.ValueBytes(extra![0])), meter);
                break;
            case "count": break;
            case "min": case "max":
                if (fold.Value.IsNull || (aggregate.Name == "min" ? CollationRules.Compare(value, fold.Value, aggregate.Collation, meter) < 0 : CollationRules.Compare(value, fold.Value, aggregate.Collation, meter) > 0)) fold.Value = value;
                break;
            case "bool_and": case "every":
                fold.Value = SqlValue.Boolean((fold.Value.IsNull || fold.Value.AsBoolean()) && value.AsBoolean()); break;
            case "bool_or":
                fold.Value = SqlValue.Boolean((!fold.Value.IsNull && fold.Value.AsBoolean()) || value.AsBoolean()); break;
            case "sum": case "avg":
                if (fold.IntegerSum is { } integerSum) integerSum.Add(value.AsInteger(), meter);
                else if (aggregate.Type == SqlType.Decimal)
                {
                    var addend = Resolve.Cast(value, SqlType.Decimal).AsDecimal();
                    meter.Charge(fold.Sum.DecimalWork(addend, "+")); meter.Guard();
                    fold.Sum = fold.Sum.AddUncapped(addend);
                }
                else if (aggregate.Type.IsFloat())
                {
                    fold.Floating!.Add(value.AsFloat(), aggregate.Type == SqlType.F32);
                }
                else
                {
                    try { fold.Value = SqlValue.Integer(checked((fold.Value.IsNull ? 0 : fold.Value.AsInteger()) + value.AsInteger()), aggregate.Type); }
                    catch (OverflowException) { throw Errors.Overflow(); }
                }
                break;
        }
    }
    internal static SqlValue Finalize(AggregatePlan aggregate, Fold fold)
    {
        if (fold.Moments is { } moments) return moments.Result(aggregate);
        if (fold.Json is { } json) return json.Result(aggregate);
        if (fold.Floating is { } floating) return floating.Result(aggregate, fold.Count);
        if (aggregate.Name == "array_agg") return ArrayResult(aggregate, fold);
        if (aggregate.Name == "count") return SqlValue.Integer(fold.Count);
        if (fold.Count == 0) return SqlValue.Null(aggregate.Type);
        if (aggregate.Name == "string_agg") return aggregate.Type == SqlType.Text ? SqlValue.Text(fold.Text.ToString()) : SqlValue.Bytes(fold.Bytes.ToArray());
        if (aggregate.Type == SqlType.Decimal && aggregate.Name is "sum" or "avg")
        {
            var sum = fold.Sum;
            return SqlValue.Decimal(aggregate.Name == "avg" ? sum.Divide(ExactDecimal.FromInt64(fold.Count)) : sum.CheckCap());
        }
        return fold.Value;
    }

    private static SqlValue ArrayResult(AggregatePlan aggregate, Fold fold)
    {
        if (fold.Collected.Count == 0) return SqlValue.Null(aggregate.Type);
        if (aggregate.Argument!.Type.Kind != SqlKind.Array) return SqlValue.Array(new(aggregate.Type.Element!, fold.Collected.Select(value => value.IsNull ? SqlValue.Null(aggregate.Type.Element!) : value)));
        var first = fold.Collected[0].AsArray();
        return SqlValue.Array(new(first.ElementType, fold.Collected.SelectMany(value => value.AsArray().Values), new[] { fold.Collected.Count }.Concat(first.Dimensions), new[] { 1 }.Concat(first.LowerBounds)));
    }
    private static int CompareInput(SortPlan[] order, Input a, Input b)
    {
        for (var i = 0; i < order.Length; i++)
        {
            var compare = SortValue.Compare(a.Keys[i], b.Keys[i], order[i]);
            if (compare != 0) return compare;
        }
        return a.Ordinal.CompareTo(b.Ordinal);
    }
    internal static void ChargeKey(SqlValue value, Meter meter)
    {
        var work = QueryExecution.KeyBytes(value);
        if (work == 0) return;
        meter.Charge(work * Costs.HashKey); meter.Guard();
    }
    private static byte[] IdentityBytes(SqlValue value)
    {
        if (value.IsNull) return [1];
        if (value.Type == SqlType.Jsonb) return new byte[] { 0 }.Concat(JsonOps.IdentityBytes(value.AsJsonb())).ToArray();
        if (value.Type.Kind is not (SqlKind.Array or SqlKind.Composite)) return KeyEncoding.Encode(value, nullable: true);
        var writer = new WireWriter(); writer.Byte(0);
        void Item(SqlValue item) { var bytes = IdentityBytes(item); writer.U32((uint)bytes.Length); writer.Bytes(bytes); }
        if (value.Type.Kind == SqlKind.Array)
        {
            var array = value.AsArray(); writer.U32((uint)array.Rank);
            for (var i = 0; i < array.Rank; i++) { writer.U32(unchecked((uint)array.Dimensions[i])); writer.U32(unchecked((uint)array.LowerBounds[i])); }
            foreach (var item in array.Values) Item(item);
        }
        else foreach (var field in value.AsComposite().Fields) Item(field);
        return writer.ToArray();
    }
    internal static string Identity(IReadOnlyList<SqlValue> values) => Convert.ToHexString(EncodedIdentity(values));
    internal static byte[] EncodedIdentity(IReadOnlyList<SqlValue> values)
    {
        if (values.Count == 0) return Array.Empty<byte>();
        if (IntegerIdentity(values) is { } fixedKey) return fixedKey;
        var writer = new WireWriter();
        for (var index = 0; index < values.Count; index++)
        {
            var bytes = IdentityBytes(values[index]);
            writer.U32((uint)bytes.Length); writer.Bytes(bytes);
        }
        return writer.ToArray();
    }
    internal static byte[] EncodedIdentity(SqlValue first, IReadOnlyList<SqlValue> rest)
    {
        if (IntegerIdentity(rest, first, includeFirst: true) is { } fixedKey) return fixedKey;
        var writer = new WireWriter();
        var bytes = IdentityBytes(first);
        writer.U32((uint)bytes.Length); writer.Bytes(bytes);
        for (var index = 0; index < rest.Count; index++)
        {
            bytes = IdentityBytes(rest[index]);
            writer.U32((uint)bytes.Length); writer.Bytes(bytes);
        }
        return writer.ToArray();
    }

    // Integer/NULL tuples need only one owned allocation. Keep the canonical
    // framing and integer encoder shared with all other grouping/key paths.
    private static byte[]? IntegerIdentity(IReadOnlyList<SqlValue> values, SqlValue first = default, bool includeFirst = false)
    {
        static int Width(SqlValue value) => value.IsNull ? 0 : value.Type.IsInteger() ? StoredValues.ScalarWidth(value.Type) : -1;
        var firstWidth = includeFirst ? Width(first) : 0;
        if (firstWidth < 0) return null;
        var length = includeFirst ? 5 + firstWidth : 0;
        for (var i = 0; i < values.Count; i++)
        {
            var width = Width(values[i]);
            if (width < 0) return null;
            length = checked(length + 5 + width);
        }
        var result = new byte[length];
        var offset = 0;
        void Write(SqlValue value)
        {
            var width = Width(value);
            System.Buffers.Binary.BinaryPrimitives.WriteUInt32BigEndian(result.AsSpan(offset), (uint)(width + 1));
            result[offset + 4] = value.IsNull ? (byte)1 : (byte)0;
            if (width != 0) KeyEncoding.WriteInteger(result.AsSpan(offset + 5, width), value.AsInteger());
            offset += 5 + width;
        }
        if (includeFirst) Write(first);
        for (var i = 0; i < values.Count; i++) Write(values[i]);
        return result;
    }
}
