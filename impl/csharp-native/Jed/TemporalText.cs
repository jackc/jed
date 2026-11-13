namespace Jed;

internal static class TemporalText
{
    internal static SqlValue Parse(string input, SqlType type, TemporalContext context)
    {
        if (type.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range) return OpenInput.Parse(input, type, context: context);
        if (type != SqlType.Timestamptz) return Resolve.ParseText(input, type);
        var parsed = TemporalInput.ParseTimestampInput(input);
        return SqlValue.Timestamp(parsed.Infinite ? parsed.Micros : parsed.HasOffset ? TemporalValue.FiniteTimestamp((Int128)parsed.Micros - (long)parsed.OffsetSeconds * TemporalValue.MicrosPerSecond) : context.ResolveZone().ToInstant(parsed.Micros), true);
    }
    internal static SqlValue Cast(SqlValue value, SqlType target, Meter meter)
    {
        if (value.IsNull) return SqlValue.Null(target);
        if (target == SqlType.Text) return SqlValue.Text(TemporalRender.CastText(value, meter));
        if (value.Type == SqlType.Text && target == SqlType.Date) return Date(value.AsText(), meter);
        if (value.Type == SqlType.Text && target == SqlType.Timestamptz)
        {
            var parsed = TemporalInput.ParseTimestampInput(value.AsText());
            if (!parsed.Infinite && !parsed.HasOffset) { meter.Charge(Costs.Timezone); meter.Guard(); }
            return Parse(value.AsText(), target, meter.Temporal);
        }
        if (value.Type == SqlType.Text && target.Kind is SqlKind.Array or SqlKind.Composite or SqlKind.Range)
            return Parse(value.AsText(), target, meter.Temporal);
        if (value.Type == SqlType.Date && target == SqlType.Timestamptz)
        {
            var wall = TemporalValue.DateToTimestamp(value.AsDate());
            if (TemporalValue.IsInfinite(wall)) return SqlValue.Timestamp(wall, true);
            meter.Charge(Costs.Timezone); meter.Guard(); return SqlValue.Timestamp(meter.Temporal.ResolveZone().ToInstant(wall), true);
        }
        if (value.Type == SqlType.Timestamptz && target.Kind is SqlKind.Timestamp or SqlKind.Date || value.Type == SqlType.Timestamp && target == SqlType.Timestamptz)
        {
            var micros = value.AsTimestamp();
            if (!TemporalValue.IsInfinite(micros))
            {
                meter.Charge(Costs.Timezone); meter.Guard();
                micros = value.Type == SqlType.Timestamptz ? meter.Temporal.ResolveZone().ToLocal(micros) : meter.Temporal.ResolveZone().ToInstant(micros);
            }
            return target == SqlType.Date ? SqlValue.Date(TemporalValue.TimestampToDate(micros)) : SqlValue.Timestamp(micros, target == SqlType.Timestamptz);
        }
        if (value.Type.Kind == SqlKind.Array && target.Kind == SqlKind.Array && value.Type != target)
        {
            var array = value.AsArray();
            return SqlValue.Array(new(target.Element!, array.Values.Select(element => Cast(element, target.Element!, meter)), array.Dimensions, array.LowerBounds, true));
        }
        return Resolve.Cast(value, target);
    }
    internal static bool IsClockDate(string input) => ClockDayOffset(input) is not null;
    private static int? ClockDayOffset(string input) => TextEncoding.FoldName(input.Trim(' ', '\t', '\r', '\n', '\f', '\v')) switch
    { "today" or "now" => 0, "tomorrow" => 1, "yesterday" => -1, _ => null };
    internal static SqlValue Date(string input, Meter meter)
    {
        var shift = ClockDayOffset(input);
        if (shift is null) return SqlValue.Date(TemporalInput.ParseDate(input));
        meter.Charge(Costs.Timezone); meter.Guard();
        var local = (Int128)meter.Temporal.ResolveZone().ToLocal(meter.Temporal.StatementMicros);
        var day = (long)(local / TemporalValue.MicrosPerDay - (local % TemporalValue.MicrosPerDay < 0 ? 1 : 0));
        return SqlValue.Date(TemporalValue.FiniteDate(day + shift.Value));
    }
}
