namespace Jed;

internal static class TemporalRender
{
    internal static long ZoneCount(SqlValue value)
    {
        if (value.IsNull) return 0;
        return value.Type.Kind switch
        {
            SqlKind.Timestamptz => TemporalValue.IsInfinite(value.AsTimestamp()) ? 0 : 1,
            SqlKind.Array => value.AsArray().Values.Sum(ZoneCount),
            SqlKind.Composite => value.AsComposite().Fields.Sum(ZoneCount),
            SqlKind.Range => (value.AsRange().Lower is { } lower ? ZoneCount(lower) : 0) + (value.AsRange().Upper is { } upper ? ZoneCount(upper) : 0),
            _ => 0
        };
    }
    internal static string Value(SqlValue value, TemporalContext context, bool element = false)
    {
        if (value.IsNull) return "NULL";
        string Child(SqlValue field) => Value(field, context, true);
        return value.Type.Kind switch
        {
            SqlKind.Timestamptz => TemporalInput.RenderTimestamp(value.AsTimestamp(), true, TemporalValue.IsInfinite(value.AsTimestamp()) ? 0 : context.ResolveZone().OffsetAt(value.AsTimestamp()).Seconds),
            SqlKind.Array => OpenInput.RenderArray(value.AsArray(), Child),
            SqlKind.Composite => OpenInput.RenderComposite(value.AsComposite(), Child),
            SqlKind.Range => OpenInput.RenderRange(value.AsRange(), Child),
            SqlKind.Boolean when element => value.AsBoolean() ? "t" : "f",
            _ => value.ToString()
        };
    }
    internal static string CastText(SqlValue value, Meter meter)
    {
        meter.Charge(ZoneCount(value) * Costs.Timezone); meter.Guard(); return Value(value, meter.Temporal);
    }
}
