namespace Jed;

/// <summary>A canonical SQL range with independent unbounded endpoints and an explicit empty value.</summary>
public sealed class SqlRange
{
    public SqlType Type { get; }
    public bool IsEmpty { get; }
    public SqlValue? Lower { get; }
    public SqlValue? Upper { get; }
    public bool LowerInclusive { get; }
    public bool UpperInclusive { get; }
    private SqlRange(SqlType type) { RequireType(type); Type = type; IsEmpty = true; }
    public static SqlRange Empty(SqlType type) => new(type);
    public SqlRange(SqlType type, SqlValue? lower, SqlValue? upper, bool lowerInclusive = true, bool upperInclusive = false)
    {
        RequireType(type); Type = type;
        if (lower is { IsNull: true }) lower = null;
        if (upper is { IsNull: true }) upper = null;
        if (lower is { } lo && lo.Type != type.Element || upper is { } hi && hi.Type != type.Element) throw Errors.Type("range bound type mismatch");
        if (lower is { } a && upper is { } b && Evaluate.Compare(a, b) > 0) throw new SqlException(SqlStates.DataException, "range lower bound exceeds upper bound");
        var discrete = RangeCatalog.Facts.Single(f => f.Element == type.Element).Discrete;
        if (discrete)
        {
            if (lower is { } l && !lowerInclusive && Step(l, out var nextLower)) { lower = nextLower; lowerInclusive = true; }
            if (upper is { } u && upperInclusive && Step(u, out var nextUpper)) { upper = nextUpper; upperInclusive = false; }
        }
        if (lower is { } canonicalLower && upper is { } canonicalUpper)
        {
            var order = Evaluate.Compare(canonicalLower, canonicalUpper);
            if (order > 0 || order == 0 && !(lowerInclusive && upperInclusive)) { IsEmpty = true; return; }
        }
        Lower = lower; Upper = upper; LowerInclusive = lower.HasValue && lowerInclusive; UpperInclusive = upper.HasValue && upperInclusive;
    }
    internal static SqlRange Stored(SqlType type, SqlValue? lower, SqlValue? upper, bool lowerInclusive, bool upperInclusive)
        => new(type, lower, upper, lowerInclusive, upperInclusive, true);
    private SqlRange(SqlType type, SqlValue? lower, SqlValue? upper, bool lowerInclusive, bool upperInclusive, bool stored)
    {
        RequireType(type); Type = type; Lower = lower; Upper = upper;
        LowerInclusive = lowerInclusive; UpperInclusive = upperInclusive;
    }
    private static void RequireType(SqlType type)
    {
        if (type.Kind != SqlKind.Range || !RangeCatalog.Facts.Any(f => f.Element == type.Element)) throw Errors.Type("invalid range type");
    }
    private static bool Step(SqlValue value, out SqlValue result)
    {
        result = value;
        if (value.Type == SqlType.Date)
        {
            var days = value.AsDate();
            if (TemporalValue.IsInfiniteDate(days)) return false;
            if (days == int.MaxValue - 1) throw Errors.Overflow();
            result = SqlValue.Date(days + 1); return true;
        }
        if (value.AsInteger() == SqlTypes.Fact(value.Type).Max) throw Errors.Overflow();
        result = SqlValue.Integer(value.AsInteger() + 1, value.Type); return true;
    }
    public override string ToString() => OpenInput.RenderRange(this);
}
