namespace Jed;

internal static class OpenValue
{
    internal static SqlType Common(SqlType left, SqlType right)
    {
        if (left == right) return left;
        if (left.Kind == SqlKind.Array && right.Kind == SqlKind.Array) return SqlType.Array(Resolve.Common(left.Element!, right.Element!));
        if (left.Kind == SqlKind.Composite && right.Kind == SqlKind.Composite && left.Identity == "record" && right.Identity == "record" && left.Fields.Count == right.Fields.Count)
            return SqlType.Record(left.Fields.Select((field, index) => new SqlField(field.Name, Resolve.Common(field.Type, right.Fields[index].Type))));
        throw Errors.Type("incompatible open value types");
    }
    internal static bool CanCast(SqlType from, SqlType to)
    {
        if (from == to || from == SqlType.Unknown) return true;
        if (from == SqlType.Text && to.Kind is SqlKind.Array or SqlKind.Range or SqlKind.Composite || to == SqlType.Text && from.Kind is SqlKind.Array or SqlKind.Range or SqlKind.Composite) return true;
        if (from.Kind == SqlKind.Array && to.Kind == SqlKind.Array)
        {
            if (from.Element!.Kind is SqlKind.Composite or SqlKind.Range or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath || to.Element!.Kind is SqlKind.Composite or SqlKind.Range or SqlKind.Json or SqlKind.Jsonb or SqlKind.Jsonpath)
                throw Errors.Unsupported("container element array cast");
            return CastCatalog.Pairs.Contains((from.Element!, to.Element!));
        }
        if (from.Kind == SqlKind.Composite && to.Kind == SqlKind.Composite) throw new SqlException(SqlStates.CannotCoerce, "composite cast requires a row constructor");
        return false;
    }
    internal static SqlValue Cast(SqlValue value, SqlType target)
    {
        if (value.IsNull) return SqlValue.Null(target);
        if (value.Type == target) return value;
        if (target == SqlType.Text) return SqlValue.Text(value.ToString());
        if (value.Type == SqlType.Text) return OpenInput.Parse(value.AsText(), target);
        if (value.Type.Kind == SqlKind.Array && target.Kind == SqlKind.Array)
        {
            var source = value.AsArray();
            return SqlValue.Array(new(target.Element!, source.Values.Select(element => Resolve.Cast(element, target.Element!)), source.Dimensions, source.LowerBounds, true));
        }
        if (value.Type.Kind == SqlKind.Composite && target.Kind == SqlKind.Composite)
        {
            var source = value.AsComposite();
            if (source.Fields.Count != target.Fields.Count) throw Errors.Type("composite field count mismatch");
            var fields = source.Fields.Select((field, index) => Resolve.ApplyModifier(Resolve.Cast(field, target.Fields[index].Type), new(target.Fields[index].Type, target.Fields[index].Precision, target.Fields[index].Scale, target.Fields[index].Length))).ToArray();
            for (var i = 0; i < fields.Length; i++) if (fields[i].IsNull && target.Fields[i].NotNull) throw new SqlException(SqlStates.NotNullViolation, "null composite field");
            return SqlValue.Composite(new(target, fields));
        }
        throw Errors.Type("invalid open value cast");
    }
    internal static long PayloadBytes(SqlValue value)
    {
        if (value.IsNull) return 0;
        return value.Type.Kind switch
        {
            SqlKind.Text or SqlKind.Json or SqlKind.Jsonpath => value.TextBytes,
            SqlKind.Jsonb => value.AsJsonb().PayloadBytes - 32,
            SqlKind.Bytea => value.ByteSpan.Length,
            SqlKind.Decimal => 4L * value.AsDecimal().ExportBase10000().Length,
            SqlKind.Array => 16L * value.AsArray().Rank + value.AsArray().Values.Sum(element => 32L + PayloadBytes(element)),
            SqlKind.Composite => value.AsComposite().Fields.Sum(field => 32L + PayloadBytes(field)),
            SqlKind.Range => RangePayload(value.AsRange()),
            _ => 0
        };
    }
    private static long RangePayload(SqlRange range) => range.IsEmpty ? 0 :
        (range.Lower is { } lower ? 32 + PayloadBytes(lower) : 0) + (range.Upper is { } upper ? 32 + PayloadBytes(upper) : 0);
    // Container comparison is one SQL operator: inner scalar comparisons do not add operator costs.
    internal static int Compare(SqlValue left, SqlValue right)
    {
        if (left.IsNull || right.IsNull) return left.IsNull == right.IsNull ? 0 : left.IsNull ? 1 : -1;
        if (left.Type != right.Type && !(left.Type.Kind == SqlKind.Array && right.Type.Kind == SqlKind.Array && left.Type.Element!.IsInteger() && right.Type.Element!.IsInteger()) && !(left.Type.Kind == SqlKind.Composite && right.Type.Kind == SqlKind.Composite && left.Type.Identity == "record" && right.Type.Identity == "record" && left.Type.Fields.Count == right.Type.Fields.Count)) throw Errors.Type("incomparable open values");
        if (left.Type.Kind == SqlKind.Array)
        {
            var a = left.AsArray(); var b = right.AsArray();
            for (var i = 0; i < Math.Min(a.Count, b.Count); i++)
            { var order = CompareElement(a.Values[i], b.Values[i]); if (order != 0) return order; }
            var count = a.Count.CompareTo(b.Count); if (count != 0) return count;
            var rank = a.Rank.CompareTo(b.Rank); if (rank != 0) return rank;
            for (var i = 0; i < a.Rank; i++)
            {
                var dimension = a.Dimensions[i].CompareTo(b.Dimensions[i]); if (dimension != 0) return dimension;
                var lower = a.LowerBounds[i].CompareTo(b.LowerBounds[i]); if (lower != 0) return lower;
            }
            return 0;
        }
        if (left.Type.Kind == SqlKind.Composite)
        {
            var a = left.AsComposite(); var b = right.AsComposite();
            for (var i = 0; i < a.Fields.Count; i++)
            { var order = CompareElement(a.Fields[i], b.Fields[i]); if (order != 0) return order; }
            return 0;
        }
        var first = left.AsRange(); var second = right.AsRange();
        if (first.IsEmpty || second.IsEmpty) return first.IsEmpty == second.IsEmpty ? 0 : first.IsEmpty ? -1 : 1;
        var lowerOrder = Bound(first.Lower, first.LowerInclusive, second.Lower, second.LowerInclusive, true);
        return lowerOrder != 0 ? lowerOrder : Bound(first.Upper, first.UpperInclusive, second.Upper, second.UpperInclusive, false);
    }
    internal static int CompareElement(SqlValue left, SqlValue right)
    {
        if (left.IsNull || right.IsNull) return left.IsNull == right.IsNull ? 0 : left.IsNull ? 1 : -1;
        return left.Type.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range ? Compare(left, right) : Evaluate.Compare(left, right);
    }
    internal static int Bound(SqlValue? a, bool aInclusive, SqlValue? b, bool bInclusive, bool lower)
    {
        if (!a.HasValue || !b.HasValue) return a.HasValue == b.HasValue ? 0 : a.HasValue == lower ? 1 : -1;
        var order = Evaluate.Compare(a.Value, b.Value);
        return order != 0 ? order : lower ? bInclusive.CompareTo(aInclusive) : aInclusive.CompareTo(bInclusive);
    }
}
