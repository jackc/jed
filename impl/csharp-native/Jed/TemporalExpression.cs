namespace Jed;

internal static class TemporalExpression
{
    internal static SqlType? Result(string op, SqlType left, SqlType right)
    {
        if (op is "*" or "/" && left == SqlType.Interval && (right.IsInteger() || right == SqlType.Decimal)) return SqlType.Interval;
        if (op == "*" && right == SqlType.Interval && (left.IsInteger() || left == SqlType.Decimal)) return SqlType.Interval;
        if (op is not ("+" or "-")) return null;
        if (left == SqlType.Date && right.IsInteger() || op == "+" && right == SqlType.Date && left.IsInteger()) return SqlType.Date;
        if (op == "-" && left == SqlType.Date && right == SqlType.Date) return SqlType.I32;
        if (left == SqlType.Interval && right == SqlType.Interval) return SqlType.Interval;
        if (left.Kind is SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz && right == SqlType.Interval)
            return left == SqlType.Date ? SqlType.Timestamp : left;
        if (op == "+" && left == SqlType.Interval && right.Kind is SqlKind.Date or SqlKind.Timestamp or SqlKind.Timestamptz)
            return right == SqlType.Date ? SqlType.Timestamp : right;
        if (op == "-" && left == right && left.Kind is SqlKind.Timestamp or SqlKind.Timestamptz) return SqlType.Interval;
        return null;
    }
    internal static SqlValue Binary(string op, SqlValue left, SqlValue right, SqlType result, Meter? meter = null)
    {
        if (op is "*" or "/")
        {
            var value = left.Type == SqlType.Interval ? left : right;
            var factor = left.Type == SqlType.Interval ? right : left;
            return SqlValue.Interval(IntervalMath.Scale(value.AsInterval(), factor.Type == SqlType.Decimal ? factor.AsDecimal() : ExactDecimal.FromInt64(factor.AsInteger()), op == "/", factor.Type.IsInteger()));
        }
        if (result == SqlType.Date)
        {
            var date = left.Type == SqlType.Date ? left.AsDate() : right.AsDate();
            var days = left.Type.IsInteger() ? left.AsInteger() : right.AsInteger();
            if (op == "-") { if (days == long.MinValue) throw TemporalValue.Overflow(); days = -days; }
            return SqlValue.Date(TemporalValue.AddDays(date, days));
        }
        if (result == SqlType.I32) return SqlValue.Integer(TemporalValue.SubtractDates(left.AsDate(), right.AsDate()), SqlType.I32);
        if (left.Type == SqlType.Interval && right.Type == SqlType.Interval)
            return SqlValue.Interval(op == "+" ? left.AsInterval().Add(right.AsInterval()) : left.AsInterval().Subtract(right.AsInterval()));
        if (result == SqlType.Interval) return SqlValue.Interval(TemporalValue.SubtractTimestamps(left.AsTimestamp(), right.AsTimestamp()));
        var time = left.Type == SqlType.Interval ? right : left;
        var interval = left.Type == SqlType.Interval ? left.AsInterval() : right.AsInterval();
        if (time.Type == SqlType.Timestamptz && !TemporalValue.IsInfinite(time.AsTimestamp()) && meter is not null)
        {
            meter.Charge(((interval.Months != 0 ? 1 : 0) + (interval.Days != 0 ? 1 : 0)) * Costs.Timezone);
            meter.Guard();
            if (op == "-") interval = interval.Negate();
            var zone = meter.Temporal.ResolveZone(); var instant = time.AsTimestamp();
            if (interval.Months != 0) instant = zone.ToInstant(TemporalValue.ShiftTimestamp(zone.ToLocal(instant), new(interval.Months, 0, 0)));
            if (interval.Days != 0) instant = zone.ToInstant(TemporalValue.ShiftTimestamp(zone.ToLocal(instant), new(0, interval.Days, 0)));
            return SqlValue.Timestamp(TemporalValue.FiniteTimestamp((Int128)instant + interval.Micros), true);
        }
        return SqlValue.Timestamp(TemporalValue.ShiftTimestamp(time.Type == SqlType.Date ? TemporalValue.DateToTimestamp(time.AsDate()) : time.AsTimestamp(), interval, op == "-"), result == SqlType.Timestamptz);
    }
}
