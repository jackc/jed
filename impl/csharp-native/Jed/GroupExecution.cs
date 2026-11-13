using System.Runtime.CompilerServices;

namespace Jed;

internal static class GroupExecution
{
    internal sealed record Input(SqlValue Value, SqlValue[] Extra, SqlValue[] Keys, int Ordinal);
    internal sealed class Fold(AggregatePlan plan)
    {
        internal readonly List<Input> Buffered = [];
        internal readonly List<SqlValue> Collected = [];
        internal readonly System.Text.StringBuilder Text = new();
        internal readonly List<byte> Bytes = [];
        internal long Count;
        internal SqlValue Value = SqlValue.Null(plan.Type);
        internal ExactDecimal Sum = ExactDecimal.FromInt64(0);
        internal readonly HashSet<string> Seen = new(StringComparer.Ordinal);
    }
    private sealed record Group(SqlValue[] Keys, Fold[] Folds);

    internal static async IAsyncEnumerable<SqlValue[]> Run(GroupPlan plan, IAsyncEnumerable<SqlValue[]> source, Resolved? filter,
        Meter meter, [EnumeratorCancellation] CancellationToken cancellationToken = default)
    {
        var groups = new List<Group>(); var positions = new Dictionary<string, int>(StringComparer.Ordinal);
        Group Make(SqlValue[] keys) => new(keys, plan.Aggregates.Select(a => new Fold(a)).ToArray());
        if (plan.Keys.Length == 0) { groups.Add(Make([])); positions.Add("", 0); }
        await foreach (var row in source.WithCancellation(cancellationToken).ConfigureAwait(false))
        {
            if (!await QueryExecution.KeepsAsync(filter, row, meter).ConfigureAwait(false)) continue;
            var keys = new SqlValue[plan.Keys.Length];
            for (var i = 0; i < keys.Length; i++) keys[i] = await Evaluate.ExprAsync(plan.Keys[i], meter, row: row).ConfigureAwait(false);
            foreach (var key in keys) ChargeKey(key, meter);
            var identity = Identity(keys);
            if (!positions.TryGetValue(identity, out var at))
            {
                at = groups.Count; positions.Add(identity, at); groups.Add(Make(keys));
            }
            var group = groups[at];
            for (var i = 0; i < plan.Aggregates.Length; i++)
            {
                var aggregate = plan.Aggregates[i]; var fold = group.Folds[i];
                if (!await QueryExecution.KeepsAsync(aggregate.Filter, row, meter).ConfigureAwait(false)) continue;
                meter.Charge(Costs.AggregateAccumulate); meter.Guard();
                var value = aggregate.Argument is null ? SqlValue.Integer(1) : await Evaluate.ExprAsync(aggregate.Argument, meter, row: row).ConfigureAwait(false);
                if (aggregate.Name == "string_agg") { meter.Charge(Costs.OperatorEval); meter.Guard(); }
                var extra = new SqlValue[aggregate.Extra?.Length ?? 0];
                for (var e = 0; e < extra.Length; e++) extra[e] = await Evaluate.ExprAsync(aggregate.Extra![e], meter, row: row).ConfigureAwait(false);
                var order = aggregate.Order ?? []; var orderKeys = new SqlValue[order.Length];
                for (var k = 0; k < order.Length; k++)
                {
                    orderKeys[k] = await Evaluate.ExprAsync(order[k].Expression, meter, row: row).ConfigureAwait(false);
                    var bytes = QueryExecution.KeyBytes(orderKeys[k]); if (bytes != 0) { meter.Charge(bytes * Costs.SortKey); meter.Guard(); }
                }
                if (aggregate.Distinct)
                {
                    ChargeKey(value, meter); foreach (var item in extra) ChargeKey(item, meter);
                    if (!fold.Seen.Add(Identity(new[] { value }.Concat(extra).ToArray()))) continue;
                }
                if (order.Length != 0) fold.Buffered.Add(new(value, extra, orderKeys, fold.Buffered.Count));
                else Accumulate(aggregate, fold, value, meter, extra);
            }
        }
        foreach (var group in groups)
        {
            var row = new SqlValue[plan.Keys.Length + plan.Aggregates.Length]; group.Keys.CopyTo(row, 0);
            for (var i = 0; i < plan.Aggregates.Length; i++)
            {
                var aggregate = plan.Aggregates[i]; var fold = group.Folds[i];
                if (aggregate.Order is { Length: > 0 } order)
                {
                    fold.Buffered.Sort((a, b) => CompareInput(order, a, b));
                    foreach (var input in fold.Buffered) Accumulate(aggregate, fold, input.Value, meter, input.Extra);
                }
                row[plan.Keys.Length + i] = Finalize(aggregate, fold);

            }
            if (await QueryExecution.KeepsAsync(plan.Having, row, meter).ConfigureAwait(false)) yield return row;
        }
    }
    internal static void Accumulate(AggregatePlan aggregate, Fold fold, SqlValue value, Meter meter, SqlValue[]? extra = null)
    {
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
            fold.Collected.Add(value); fold.Count++; return;
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
                break;
            case "count": break;
            case "min": case "max":
                if (fold.Value.IsNull || (aggregate.Name == "min" ? Evaluate.Compare(value, fold.Value, meter) < 0 : Evaluate.Compare(value, fold.Value, meter) > 0)) fold.Value = value;
                break;
            case "bool_and": case "every":
                fold.Value = SqlValue.Boolean((fold.Value.IsNull || fold.Value.AsBoolean()) && value.AsBoolean()); break;
            case "bool_or":
                fold.Value = SqlValue.Boolean((!fold.Value.IsNull && fold.Value.AsBoolean()) || value.AsBoolean()); break;
            case "sum": case "avg":
                if (aggregate.Type == SqlType.Decimal)
                {
                    var addend = Resolve.Cast(value, SqlType.Decimal).AsDecimal();
                    meter.Charge(fold.Sum.DecimalWork(addend, "+")); meter.Guard();
                    fold.Sum = fold.Sum.AddUncapped(addend);
                }
                else if (aggregate.Type.IsFloat())
                {
                    var before = fold.Value.IsNull ? 0 : fold.Value.AsFloat();
                    fold.Value = SqlValue.Float(FloatValue.Binary("+", before, value.AsFloat(), aggregate.Type == SqlType.F32), aggregate.Type);
                }
                else
                {
                    try { fold.Value = SqlValue.Integer(checked((fold.Value.IsNull ? 0 : fold.Value.AsInteger()) + value.AsInteger()), aggregate.Type); }
                    catch (OverflowException) { throw Errors.Overflow(); }
                }
                break;
        }
    }
    internal static SqlValue Finalize(AggregatePlan aggregate, Fold fold) => aggregate.Name == "array_agg" ? ArrayResult(aggregate, fold) : aggregate.Name == "string_agg" ? fold.Count == 0 ? SqlValue.Null(aggregate.Type) : aggregate.Type == SqlType.Text ? SqlValue.Text(fold.Text.ToString()) : SqlValue.Bytes(fold.Bytes.ToArray()) : aggregate.Name == "count" ? SqlValue.Integer(fold.Count) : fold.Count == 0 ? SqlValue.Null(aggregate.Type) :
                    aggregate.Type == SqlType.Decimal && aggregate.Name is "sum" or "avg" ? SqlValue.Decimal(aggregate.Name == "avg" ? fold.Sum.Divide(ExactDecimal.FromInt64(fold.Count)) : fold.Sum) :
                    aggregate.Type.IsFloat() && aggregate.Name == "avg" ? SqlValue.Float(fold.Value.AsFloat() / fold.Count, aggregate.Type) : fold.Value;

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
            var left = a.Keys[i]; var right = b.Keys[i];
            var compare = left.IsNull || right.IsNull ? left.IsNull == right.IsNull ? 0 : left.IsNull == order[i].NullsFirst ? -1 : 1 :
                order[i].Descending ? Evaluate.Compare(right, left) : Evaluate.Compare(left, right);
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
    internal static string Identity(IReadOnlyList<SqlValue> values)
    {
        if (values.Count == 0) return "";
        var writer = new WireWriter();
        foreach (var value in values)
        {
            var bytes = IdentityBytes(value);
            writer.U32((uint)bytes.Length); writer.Bytes(bytes);
        }
        return Convert.ToHexString(writer.ToArray());
    }
}
