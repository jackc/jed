namespace Jed;

// A retained comparison value plus its one-time UCA decoration. C values need no byte copy.
internal readonly record struct SortValue(SqlValue Value, byte[]? Key)
{
    internal static int Compare(SortValue left, SortValue right, SortPlan order)
    {
        if (left.Value.IsNull || right.Value.IsNull)
            return left.Value.IsNull == right.Value.IsNull ? 0 : left.Value.IsNull == order.NullsFirst ? -1 : 1;
        var result = left.Key is not null && right.Key is not null ? left.Key.AsSpan().SequenceCompareTo(right.Key) : Evaluate.Compare(left.Value, right.Value);
        return order.Descending ? -result : result;
    }
}
